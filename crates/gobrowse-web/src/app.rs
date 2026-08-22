#![allow(
    clippy::redundant_locals,
    clippy::manual_strip,
    clippy::collapsible_if,
    clippy::skip_while_next,
    clippy::clone_on_copy
)]
use gloo_net::http::Request;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};

const PENDING_SUBMISSION_KEY: &str = "gobrowse.pending-chat-submission";
const ACTIVE_RUN_KEY: &str = "gobrowse.active-chat-run";
const OPEN_TERMINAL_KEY: &str = "gobrowse.open-terminal";
const TERMINAL_POLL_MS: i32 = 1_000;
const TERMINAL_MAX_READ_BYTES: u32 = 32_768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthStage {
    Checking,
    Setup,
    Login,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Chat,
    Workspaces,
    Library,
    Tasks,
    Agents,
    Terminals,
    Skills,
    Autobiography,
    Mcp,
    Models,
    Diagnostics,
    UiPackages,
    Security,
}

#[derive(Debug, Deserialize)]
struct SetupStatus {
    owner_required: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct User {
    id: String,
    display_name: String,
    role: String,
}

#[derive(Debug, Serialize)]
struct SetupRequest<'a> {
    email: &'a str,
    display_name: &'a str,
    password: &'a str,
}

#[derive(Debug, Serialize)]
struct LoginRequest<'a> {
    email: &'a str,
    password: &'a str,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
    correlation_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BookSummary {
    id: String,
    title: String,
    #[serde(default)]
    snippet: String,
    /// Registry role; SQL NULL / absent means SOURCE.
    #[serde(default)]
    kind: Option<String>,
    /// Capability/component names from `books.metadata->>'capabilities'`.
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    book_type: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    trust: String,
}

/// `POST /library/books/{id}/load` — kind-specific progressive load. The
/// server injects `book_id`, `title`, `kind`, `book_type`, `trust` and the
/// book `revision` into the kind-specific payload; optional fields carry the
/// kind-specific content.
#[derive(serde::Deserialize, Clone, Debug)]
#[allow(dead_code)]
struct LoadedBook {
    book_id: String,
    title: String,
    kind: Option<String>,
    book_type: String,
    trust: String,
    revision: i64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    skill_id: Option<String>,
    #[serde(default)]
    mcp_server_id: Option<String>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    tools: Vec<McpToolSummary>,
    #[serde(default)]
    components: Vec<ComponentPreview>,
    #[serde(default)]
    plugin_id: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    promoted: Option<bool>,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[allow(dead_code)]
struct McpToolSummary {
    name: String,
    description: String,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[allow(dead_code)]
struct PinnedBookSummary {
    id: String,
    title: String,
    book_type: String,
    scope: String,
    trust: String,
    security_classification: String,
    updated_at: String,
}

#[derive(serde::Serialize)]
struct CreateBookBody {
    title: String,
    body: String,
    book_type: String,
    scope: String,
    tags: Vec<String>,
    provenance: String,
    trust: String,
    workspace_id: Option<String>,
    conversation_id: Option<String>,
    security_classification: String,
    metadata: serde_json::Value,
}

#[derive(serde::Deserialize, Clone, Debug)]
struct CatalogModel {
    reference: String,
    context_window: i32,
    output_limit: i32,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[allow(dead_code)]
struct CatalogProvider {
    provider_type: String,
    display_name: String,
    base_url: String,
    api_format: String,
    models: Vec<CatalogModel>,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[allow(dead_code)]
struct ModelCostRow {
    provider: String,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    runs: i64,
    spend: f64,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[allow(dead_code)]
struct ProviderCostRow {
    provider: String,
    spend: f64,
    runs: i64,
}

#[derive(serde::Deserialize, Clone, Debug)]
struct DailyCostRow {
    day: String,
    spend: f64,
}

#[derive(serde::Deserialize, Clone, Debug)]
struct UsageSummary {
    total_spend: f64,
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_runs: i64,
    unpriced_runs: i64,
    per_model: Vec<ModelCostRow>,
    per_provider: Vec<ProviderCostRow>,
    per_day: Vec<DailyCostRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct ConversationSummary {
    id: String,
    title: String,
    status: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MessageSummary {
    role: String,
    text: String,
    provider: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct PendingSubmission {
    user_id: String,
    conversation_id: String,
    client_submission_id: String,
    text: String,
    model_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ActiveRunSession {
    user_id: String,
    conversation_id: String,
    run_id: String,
}

#[derive(Debug, Serialize)]
struct StartTurnRequest<'a> {
    client_submission_id: &'a str,
    text: &'a str,
    model_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct RunSummary {
    id: String,
    state: String,
    error_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TurnResponse {
    #[serde(rename = "message_id")]
    _message_id: String,
    run: RunSummary,
}

#[derive(Debug, Deserialize)]
struct RunEvent {
    sequence: i64,
    event_type: String,
    payload: serde_json::Value,
}
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct TokenBudgetBreakdown {
    conversation: u32,
    source_books: u32,
    skill_books: u32,
    plugin_books: u32,
    mcp_schemas: u32,
    workspace: u32,
    system_policy: u32,
    total_used: u32,
    budget: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct RoutingDecision {
    book_id: String,
    book_title: String,
    book_kind: String,
    action: String, // "selected" or "omitted"
    reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ModelRoutingInfo {
    model_id: String,
    routing_reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ContextResponse {
    #[serde(default)]
    run_id: String,
    schema_version: Option<i32>,
    token_budget: Option<TokenBudgetBreakdown>,
    routing_decisions: Option<Vec<RoutingDecision>>,
    model_routing: Option<ModelRoutingInfo>,
}

#[derive(Debug, Serialize)]
struct CreateConversationRequest<'a> {
    title: &'a str,
    workspace_id: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct EmbeddingConfiguration {
    id: String,
    #[serde(default)]
    provider_id: String,
    display_name: String,
    provider_type: String,
    model_reference: String,
    dimensions: i32,
    active: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct EmbeddingJob {
    id: String,
    status: String,
    last_error_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateEmbeddingConfiguration<'a> {
    display_name: &'a str,
    provider_type: &'a str,
    base_url: &'a str,
    secret_reference: Option<&'a str>,
    model_reference: &'a str,
    dimensions: i32,
    activate: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatModelConfiguration {
    id: String,
    #[serde(default)]
    provider_id: String,
    display_name: String,
    provider_type: String,
    model_reference: String,
    context_window: i32,
    output_limit: i32,
    active: bool,
    fallback_model_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CreateChatModelConfiguration<'a> {
    display_name: &'a str,
    provider_type: &'a str,
    base_url: &'a str,
    secret_reference: Option<&'a str>,
    model_reference: &'a str,
    context_window: i32,
    output_limit: i32,
    priority: i32,
    activate: bool,
    fallback_model_ids: Vec<&'a str>,
}

#[derive(Debug, Serialize)]
struct StoreSecretPayload<'a> {
    purpose: &'a str,
    allowed_hosts: Vec<String>,
    value: String,
}

#[derive(Debug, Serialize)]
struct ProviderTestRequest<'a> {
    base_url: &'a str,
    #[serde(default)]
    api_key: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderTestResponse {
    ok: bool,
    detail: String,
    models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeleteTarget {
    Provider { id: String, label: String },
    Model { id: String, label: String },
    EmbeddingConfig { id: String, label: String },
    Workspace { id: String, label: String },
    Book { id: String, label: String },
    Skill { id: String, label: String },
    UiPackage { id: String, label: String },
}

const WORKSPACE_STORAGE_KEY: &str = "gobrowse.selected-workspace";

fn load_selected_workspace() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(WORKSPACE_STORAGE_KEY).ok().flatten())
        .filter(|v| !v.trim().is_empty())
}

fn store_selected_workspace(value: &Option<String>) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        match value {
            Some(id) if !id.trim().is_empty() => {
                let _ = storage.set_item(WORKSPACE_STORAGE_KEY, id);
            }
            _ => {
                let _ = storage.remove_item(WORKSPACE_STORAGE_KEY);
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct SecretMeta {
    id: String,
}
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct AutobiographyResponse {
    id: String,
    body: String,
    revision: i64,
    policy: String,
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DetectedProviderInfo {
    provider_type: String,
    available: bool,
    models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AutoDetectResponse {
    detected: Vec<DetectedProviderInfo>,
}

#[derive(Clone, Copy)]
struct ChatTaskSignals {
    generation: RwSignal<u64>,
    selected: RwSignal<Option<ConversationSummary>>,
    messages: RwSignal<Vec<MessageSummary>>,
    streamed: RwSignal<String>,
    tool_calls: RwSignal<Vec<ToolCallCard>>,
    active_run: RwSignal<Option<String>>,
    resolving_run: RwSignal<bool>,
    status: RwSignal<String>,
}

#[component]
pub fn App() -> impl IntoView {
    let auth = RwSignal::new(AuthStage::Checking);
    let user = RwSignal::new(None::<User>);
    let error = RwSignal::new(None::<String>);

    spawn_local(async move {
        match Request::get("/api/v1/setup").send().await {
            Ok(response) if response.ok() => match response.json::<SetupStatus>().await {
                Ok(status) if status.owner_required => auth.set(AuthStage::Setup),
                Ok(_) => match Request::get("/api/v1/auth/me").send().await {
                    Ok(response) if response.ok() => match response.json::<User>().await {
                        Ok(current) => {
                            user.set(Some(current));
                            auth.set(AuthStage::Ready);
                        }
                        Err(_) => auth.set(AuthStage::Login),
                    },
                    _ => auth.set(AuthStage::Login),
                },
                Err(_) => error.set(Some("The setup response was not valid.".into())),
            },
            _ => error.set(Some("Gobrowse OS could not reach its API.".into())),
        }
    });

    view! {
        <main>
            {move || match auth.get() {
                AuthStage::Checking => view! { <Loading /> }.into_any(),
                AuthStage::Setup => view! { <AuthPanel setup=true auth user error /> }.into_any(),
                AuthStage::Login => view! { <AuthPanel setup=false auth user error /> }.into_any(),
                AuthStage::Ready => view! { <OperatorShell user auth /> }.into_any(),
            }}
            {move || error.get().map(|message| view! {
                <div class="toast" role="alert">{message}</div>
            })}
        </main>
    }
}

#[component]
fn Loading() -> impl IntoView {
    view! {
        <section class="auth-frame" aria-busy="true">
            <div class="brand-mark">"G/OS"</div>
            <p class="utility">"Opening the index..."</p>
        </section>
    }
}

#[component]
fn AuthPanel(
    setup: bool,
    auth: RwSignal<AuthStage>,
    user: RwSignal<Option<User>>,
    error: RwSignal<Option<String>>,
) -> impl IntoView {
    let email = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let passkey_pending = RwSignal::new(false);
    let passkey_error = RwSignal::new(None::<String>);

    let submit = move |_| {
        if pending.get_untracked() {
            return;
        }
        pending.set(true);
        error.set(None);
        let email_value = email.get_untracked();
        let name_value = name.get_untracked();
        let password_value = password.get_untracked();
        spawn_local(async move {
            let result = if setup {
                Request::post("/api/v1/setup/owner").json(&SetupRequest {
                    email: &email_value,
                    display_name: &name_value,
                    password: &password_value,
                })
            } else {
                Request::post("/api/v1/auth/login").json(&LoginRequest {
                    email: &email_value,
                    password: &password_value,
                })
            };
            match result {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => match response.json::<User>().await {
                        Ok(current) => {
                            user.set(Some(current));
                            auth.set(AuthStage::Ready);
                        }
                        Err(_) => error.set(Some("The account response was not valid.".into())),
                    },
                    Ok(response) => {
                        let status = response.status();
                        let message = response.json::<ApiError>().await.map_or_else(
                            |_| format!("The request failed with HTTP {status}."),
                            |api| format!("{} Reference: {}", api.message, api.correlation_id),
                        );
                        error.set(Some(message));
                    }
                    Err(_) => error.set(Some("The server did not answer.".into())),
                },
                Err(_) => error.set(Some("The account request could not be encoded.".into())),
            }
            password.set(String::new());
            pending.set(false);
        });
    };

    let passkey_login = move |_| {
        if setup || passkey_pending.get_untracked() {
            return;
        }
        passkey_pending.set(true);
        passkey_error.set(None);
        error.set(None);
        spawn_local(async move {
            let outcome: Result<(), String> = async {
                let start_resp = Request::post("/api/v1/auth/methods/webauthn/login")
                    .json(&serde_json::json!({}))
                    .map_err(|_| "Could not start passkey login.".to_string())?
                    .send()
                    .await
                    .map_err(|_| "Passkey service did not answer.".to_string())?;
                if !start_resp.ok() {
                    return Err(api_error(&start_resp).await);
                }
                let started: WebauthnLoginStart = start_resp
                    .json()
                    .await
                    .map_err(|_| "Invalid passkey options.".to_string())?;
                let opts = serde_json::to_value(started.request_options)
                    .map_err(|_| "Invalid request options.".to_string())?;
                let credential_js = webauthn_get_credential(opts)
                    .await
                    .map_err(|e| map_webauthn_error(&e))?;
                let credential = js_credential_to_json(credential_js)?;
                let complete = Request::post("/api/v1/auth/methods/webauthn/login/complete")
                    .json(&serde_json::json!({
                        "ceremony_id": started.ceremony_id,
                        "credential": credential,
                    }))
                    .map_err(|_| "Could not encode credential.".to_string())?
                    .send()
                    .await
                    .map_err(|_| "Passkey verification did not answer.".to_string())?;
                if !complete.ok() {
                    return Err(api_error(&complete).await);
                }
                let current: User = complete
                    .json()
                    .await
                    .map_err(|_| "Invalid account response.".to_string())?;
                user.set(Some(current));
                auth.set(AuthStage::Ready);
                Ok(())
            }
            .await;
            if let Err(msg) = outcome {
                passkey_error.set(Some(msg));
            }
            passkey_pending.set(false);
        });
    };

    view! {
        <section class="auth-layout">
            <div class="auth-thesis">
                <div class="brand-mark">"G/OS"</div>
                <p class="eyebrow">"Gobrowse OS / self-hosted agent environment"</p>
                <h1>"Keep the work. Find the context. " <em>"Inspect every action."</em></h1>
                <p>"The Library turns conversations, projects, discoveries, and operating history into a searchable index without surrendering control of your data."</p>
                <div class="index-sample">
                    <span>"L-0001"</span>
                    <strong>"Autobiography"</strong>
                    <small>"PROFILE · USER PROVIDED · REV 1"</small>
                </div>
            </div>
            <form class="auth-panel" on:submit=move |event| { event.prevent_default(); submit(()); }>
                <p class="utility">{if setup { "FIRST RUN / OWNER" } else { "AUTHENTICATE / LOCAL" }}</p>
                <h2>{if setup { "Create the owner" } else { "Open your desk" }}</h2>
                {setup.then(|| view! {
                    <label>"Display name"
                        <input required maxlength="200" autocomplete="name"
                            prop:value=move || name.get()
                            on:input=move |event| name.set(event_target_value(&event)) />
                    </label>
                })}
                <label>"Email"
                    <input required type="email" maxlength="320" autocomplete="email"
                        prop:value=move || email.get()
                        on:input=move |event| email.set(event_target_value(&event)) />
                </label>
                <label>"Password"
                    <input required type="password" minlength="12" maxlength="1024"
                        autocomplete=if setup { "new-password" } else { "current-password" }
                        prop:value=move || password.get()
                        on:input=move |event| password.set(event_target_value(&event)) />
                </label>
                <button class="primary" type="submit" disabled=move || pending.get()>
                    {move || if pending.get() { "Working..." } else if setup { "Create owner" } else { "Sign in" }}
                </button>
                {(!setup).then(|| view! {
                    <div class="auth-divider"><span>"or"</span></div>
                    <button class="secondary auth-passkey" type="button"
                        disabled=move || passkey_pending.get()
                        on:click=passkey_login>
                        <span class="auth-passkey-icon">"⌖"</span>
                        {move || if passkey_pending.get() { "Waiting for authenticator..." } else { "Sign in with passkey" }}
                    </button>
                    {move || passkey_error.get().map(|m| view! { <p class="inline-error">{m}</p> })}
                })}
                <p class="form-note">"Credentials stay in Gobrowse OS. Models receive capability status, never passwords or provider secrets."</p>
            </form>
        </section>
    }
}

#[component]
fn OperatorShell(user: RwSignal<Option<User>>, auth: RwSignal<AuthStage>) -> impl IntoView {
    let page = RwSignal::new(Page::Chat);
    let user_id = user
        .get_untracked()
        .map(|current| current.id)
        .unwrap_or_default();
    let is_admin = user
        .get_untracked()
        .is_some_and(|current| matches!(current.role.as_str(), "OWNER" | "ADMIN"));
    let logout = move |_| {
        clear_chat_storage();
        spawn_local(async move {
            let _ = Request::post("/api/v1/auth/logout").send().await;
            user.set(None);
            auth.set(AuthStage::Login);
        })
    };
    view! {
        <a class="skip-link" href="#workspace">"Skip to workspace"</a>
        <div class="os-shell">
            <header class="command-bar">
                <div class="brand-mark compact">"G/OS"</div>
                <button class="library-search" on:click=move |_| page.set(Page::Library)>
                    <span>"Search the Library"</span>
                </button>
                <div class="runtime-status"><i></i><span>"SERVER READY"</span></div>
                <button class="user-control" on:click=logout>
                    {move || user.get().map_or_else(|| "Account".into(), |current| format!("{} · {}", current.display_name, current.role))}
                </button>
            </header>
            <nav class="side-nav" aria-label="Primary">
                <NavGroup title="OPERATE" items=vec![("Chat", Page::Chat), ("Tasks", Page::Tasks), ("Agents", Page::Agents), ("Terminals", Page::Terminals)] page />
                <NavGroup title="ORGANIZE" items=vec![("Library", Page::Library), ("Workspaces", Page::Workspaces), ("Skills", Page::Skills), ("Autobiography", Page::Autobiography)] page />
                <NavGroup title="DISPLAY" items=vec![("UI Packages", Page::UiPackages)] page />
                <NavGroup title="CONNECT" items=if is_admin { vec![("MCP", Page::Mcp), ("Models", Page::Models)] } else { vec![("MCP", Page::Mcp)] } page />
                <NavGroup title="ACCOUNT" items=vec![("Security", Page::Security)] page />
                {is_admin.then(|| view! { <NavGroup title="INSPECT" items=vec![("Diagnostics", Page::Diagnostics)] page /> })}
            </nav>
            <section id="workspace" class="workspace" tabindex="-1">
                <div style:display=move || if page.get() == Page::Chat { "block" } else { "none" }>
                    <ChatPage user_id=user_id.clone() />
                </div>
                <div style:display=move || if page.get() == Page::Library { "block" } else { "none" }>
                    <LibraryPage initial_kind=None page />
                </div>
                <div style:display=move || if page.get() == Page::Skills { "block" } else { "none" }>
                    <LibraryPage initial_kind=Some("SKILL") page />
                </div>
                <div style:display=move || if page.get() == Page::Mcp { "block" } else { "none" }>
                    <LibraryPage initial_kind=Some("MCP") page />
                </div>
                <div style:display=move || if page.get() == Page::Autobiography { "block" } else { "none" }>
                    <AutobiographyPage />
                </div>
                <div style:display=move || if page.get() == Page::Workspaces { "block" } else { "none" }>
                    <WorkspacesPage />
                </div>
                <div style:display=move || if page.get() == Page::Terminals { "block" } else { "none" }>
                    <TerminalsPage />
                </div>
                <div style:display=move || if page.get() == Page::Models { "block" } else { "none" }>
                    <ModelsPage />
                </div>
                <div style:display=move || if page.get() == Page::Diagnostics { "block" } else { "none" }>
                    <DiagnosticsPage />
                </div>
                <div style:display=move || if page.get() == Page::UiPackages { "block" } else { "none" }>
                    <UiPackagesPage />
                </div>
                <div style:display=move || if page.get() == Page::Security { "block" } else { "none" }>
                    <SecurityPage user=user />
                </div>
                <div style:display=move || if matches!(page.get(), Page::Tasks | Page::Agents) { "block" } else { "none" }>
                    {move || {
                        let p = page.get();
                        if matches!(p, Page::Tasks | Page::Agents) {
                            view! { <EmptyOperationalPage page=p /> }.into_any()
                        } else {
                            view! { <span></span> }.into_any()
                        }
                    }}
                </div>
            </section>
            <aside class="context-pane">
                <p class="utility">"ACTIVE CONTEXT"</p>
                <h3>"No workspace selected"</h3>
                <p>"Choose a workspace to attach its Books, files, tasks, and sandbox policy to this view."</p>
                <button class="text-button" on:click=move |_| page.set(Page::Workspaces)>"Choose workspace →"</button>
                <hr />
                <p class="utility">"ACTIVITY LEDGER"</p>
                <div class="empty-small">"Agent and worktree events will stream here."</div>
            </aside>
            <nav class="mobile-nav" aria-label="Mobile primary">
                <button on:click=move |_| page.set(Page::Chat)>"Chat"</button>
                <button on:click=move |_| page.set(Page::Library)>"Library"</button>
                <button on:click=move |_| page.set(Page::Tasks)>"Tasks"</button>
                <button on:click=move |_| page.set(if is_admin { Page::Diagnostics } else { Page::Workspaces })>"More"</button>
            </nav>
        </div>
    }
}

#[component]
fn NavGroup(
    title: &'static str,
    items: Vec<(&'static str, Page)>,
    page: RwSignal<Page>,
) -> impl IntoView {
    view! {
        <div class="nav-group">
            <p>{title}</p>
            {items.into_iter().map(|(label, target)| view! {
                <button class:active=move || page.get() == target on:click=move |_| page.set(target)>{label}</button>
            }).collect_view()}
        </div>
    }
}

#[component]
fn PinnedContext(conversation_id: String) -> impl IntoView {
    let lifecycle = Arc::new(AtomicBool::new(true));
    on_cleanup({
        let lifecycle = Arc::clone(&lifecycle);
        move || lifecycle.store(false, Ordering::Release)
    });
    let pins = RwSignal::new(Vec::<PinnedBookSummary>::new());
    let library = RwSignal::new(Vec::<BookSummary>::new());
    let open = RwSignal::new(false);
    let status = RwSignal::new(String::new());

    let toggle = {
        let lifecycle = Arc::clone(&lifecycle);
        let conversation_id = conversation_id.clone();
        move |event: leptos::ev::MouseEvent| {
            event.prevent_default();
            let should_open = !open.get_untracked();
            open.set(should_open);
            let lifecycle = Arc::clone(&lifecycle);
            let conversation_id = conversation_id.clone();
            if should_open {
                spawn_local(async move {
                    let books_response = Request::get("/api/v1/library/books").send().await;
                    if !lifecycle_is_active(&lifecycle) {
                        return;
                    }
                    if let Ok(response) = books_response
                        && response.ok()
                        && let Ok(list) = response.json::<Vec<BookSummary>>().await
                    {
                        library.set(list);
                    }
                    refresh_pins(&conversation_id, pins, status, lifecycle);
                });
            }
        }
    };

    let pin_book = {
        let lifecycle = Arc::clone(&lifecycle);
        let conversation_id = conversation_id.clone();
        move |book_id: String| {
            let lifecycle = Arc::clone(&lifecycle);
            let conversation_id = conversation_id.clone();
            spawn_local(async move {
                let response = Request::put(&format!(
                    "/api/v1/conversations/{conversation_id}/pins/{book_id}"
                ))
                .send()
                .await;
                if !lifecycle_is_active(&lifecycle) {
                    return;
                }
                match response {
                    Ok(response) if response.ok() => {}
                    _ => status.set("Pin failed".into()),
                }
                refresh_pins(&conversation_id, pins, status, lifecycle);
            });
        }
    };

    let unpin = {
        let lifecycle = Arc::clone(&lifecycle);
        let conversation_id = conversation_id.clone();
        move |book_id: String| {
            let lifecycle = Arc::clone(&lifecycle);
            let conversation_id = conversation_id.clone();
            spawn_local(async move {
                let response = Request::delete(&format!(
                    "/api/v1/conversations/{conversation_id}/pins/{book_id}"
                ))
                .send()
                .await;
                if !lifecycle_is_active(&lifecycle) {
                    return;
                }
                match response {
                    Ok(response) if response.ok() => {}
                    _ => status.set("Unpin failed".into()),
                }
                refresh_pins(&conversation_id, pins, status, lifecycle);
            });
        }
    };

    view! {
        <div class="pin-context">
            <button class="text-button" on:click=toggle>
                {move || if open.get() { "Close pinned context" } else { "Pin Library books" }}
            </button>
            <p class="form-note">{move || status.get()}</p>
            {move || if open.get() {
                let unpin = unpin.clone();
                let pin_book = pin_book.clone();
                view! {
                    <div class="pins-section">
                        <p class="utility">"PINNED TO THIS CONVERSATION"</p>
                        {move || if pins.get().is_empty() {
                            view! { <div class="empty-small">"No pinned books yet. Pick from the Library below."</div> }.into_any()
                        } else {
                            pins.get().into_iter().map(|pin| {
                                let id = pin.id.clone();
                                let title = pin.title.clone();
                                let unpin = unpin.clone();
                                view! {
                                    <div class="index-row">
                                        <strong>{title}</strong>
                                        <span class="spine-cell">{pin.security_classification.to_uppercase()}</span>
                                        <button type="button" on:click=move |_| unpin(id.clone())>"Remove"</button>
                                    </div>
                                }
                            }).collect_view().into_any()
                        }}
                    </div>
                    <div class="pins-section">
                        <p class="utility">"LIBRARY"</p>
                        {move || library.get().into_iter().map(|book| {
                            let id = book.id.clone();
                            let title = book.title.clone();
                            let pinned = pins.get().iter().any(|pin| pin.id == id);
                            let pin_book = pin_book.clone();
                            view! {
                                <div class="index-row">
                                    <strong>{title}</strong>
                                    <span class="spine-cell">{book.book_type}</span>
                                    {if pinned {
                                        view! { <span class="utility">"PINNED"</span> }.into_any()
                                    } else {
                                        view! { <button type="button" on:click=move |_| pin_book(id.clone())>"Pin"</button> }.into_any()
                                    }}
                                </div>
                            }
                        }).collect_view()}
                    </div>
                }.into_any()
            } else {
                view! { <span class="utility"></span> }.into_any()
            }}
        </div>
    }
}

fn refresh_pins(
    conversation_id: &str,
    pins: RwSignal<Vec<PinnedBookSummary>>,
    status: RwSignal<String>,
    lifecycle: Arc<AtomicBool>,
) {
    let conversation_id = conversation_id.to_owned();
    spawn_local(async move {
        let response = Request::get(&format!("/api/v1/conversations/{conversation_id}/pins"))
            .send()
            .await;
        if !lifecycle_is_active(&lifecycle) {
            return;
        }
        match response {
            Ok(response) if response.ok() => {
                if let Ok(list) = response.json::<Vec<PinnedBookSummary>>().await {
                    pins.set(list);
                }
            }
            _ => status.set("Could not load pinned context".into()),
        }
    });
}

#[component]
fn ChatPage(user_id: String) -> impl IntoView {
    let lifecycle = Arc::new(AtomicBool::new(true));
    on_cleanup({
        let lifecycle = Arc::clone(&lifecycle);
        move || lifecycle.store(false, Ordering::Release)
    });
    let restored_submission = load_pending_submission(&user_id);
    let restored_run = load_active_run(&user_id);
    let conversations = RwSignal::new(Vec::<ConversationSummary>::new());
    let selected = RwSignal::new(restored_run.as_ref().map(|run| ConversationSummary {
        id: run.conversation_id.clone(),
        title: "Restoring conversation...".into(),
        status: "active".into(),
    }));
    let messages = RwSignal::new(Vec::<MessageSummary>::new());
    let chat_models = RwSignal::new(Vec::<ChatModelConfiguration>::new());
    let workspaces = RwSignal::new(Vec::<WorkspaceSummary>::new());
    let selected_workspace = RwSignal::new(load_selected_workspace());
    Effect::new(move |_| {
        store_selected_workspace(&selected_workspace.get());
    });
    load_workspaces(workspaces);
    let title = RwSignal::new(String::new());
    let draft = RwSignal::new(
        restored_submission
            .as_ref()
            .filter(|submission| {
                restored_run
                    .as_ref()
                    .is_none_or(|run| run.conversation_id == submission.conversation_id)
            })
            .map_or_else(String::new, |submission| submission.text.clone()),
    );
    let streamed = RwSignal::new(String::new());
    let tool_calls = RwSignal::new(Vec::<ToolCallCard>::new());
    let transcript_ref = NodeRef::<leptos::html::Div>::new();
    let transcript_scroll = RwSignal::new(0_i32);
    let active_run = RwSignal::new(restored_run.as_ref().map(|run| run.run_id.clone()));
    let resolving_run = RwSignal::new(restored_run.is_some());
    let submitting = RwSignal::new(false);
    let pending_submission = RwSignal::new(restored_submission);
    let generation = RwSignal::new(0_u64);
    let status = RwSignal::new(String::new());
    Effect::new(move |_| {
        let _ = streamed.get();
        let _ = tool_calls.get();
        let _ = messages.get();
        if let Some(el) = transcript_ref.get() {
            let near_bottom = (el.scroll_height() - el.scroll_top() - el.client_height()) < 120;
            if near_bottom {
                el.set_scroll_top(el.scroll_height());
            }
        }
    });
    Effect::new(move |_| {
        let top = transcript_scroll.get();
        if let Some(el) = transcript_ref.get() {
            el.set_scroll_top(top);
        }
    });
    Effect::new(move |_| {
        let shared = active_conversation();
        match selected.get() {
            Some(conversation) => shared.set(Some(conversation)),
            None => shared.set(None),
        }
    });
    load_conversations(conversations, status, Arc::clone(&lifecycle));
    load_chat_models(chat_models, status, Arc::clone(&lifecycle));
    if let Some(restored_run) = restored_run {
        let task_generation = advance_generation(generation);
        status.set("Restoring active run...".into());
        load_conversation_summary(
            &restored_run.conversation_id,
            task_generation,
            generation,
            selected,
            Arc::clone(&lifecycle),
        );
        load_messages(
            &restored_run.conversation_id,
            task_generation,
            generation,
            selected,
            messages,
            status,
            Arc::clone(&lifecycle),
        );
        resolve_active_run(
            user_id.clone(),
            restored_run.conversation_id,
            task_generation,
            ChatTaskSignals {
                generation,
                selected,
                messages,
                streamed,
                tool_calls,
                active_run,
                resolving_run,
                status,
            },
            Arc::clone(&lifecycle),
        );
    }
    let create_lifecycle = Arc::clone(&lifecycle);
    let create = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = title.get_untracked();
        if value.trim().is_empty() {
            return;
        }
        let workspace_id = selected_workspace.get_untracked();
        let workspace_id = workspace_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_owned());
        status.set("Creating conversation...".into());
        let lifecycle = Arc::clone(&create_lifecycle);
        spawn_local(async move {
            let request = Request::post("/api/v1/conversations").json(&CreateConversationRequest {
                title: value.trim(),
                workspace_id: workspace_id.as_deref(),
            });
            match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        let conversation = response.json::<ConversationSummary>().await;
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        match conversation {
                            Ok(conversation) => {
                                title.set(String::new());
                                advance_generation(generation);
                                selected.set(Some(conversation));
                                messages.set(Vec::new());
                                streamed.set(String::new());
                                active_run.set(None);
                                resolving_run.set(false);
                                submitting.set(false);
                                status.set("Conversation ready.".into());
                                load_conversations(conversations, status, Arc::clone(&lifecycle));
                            }
                            Err(_) => status.set("Conversation response was not valid.".into()),
                        }
                    }
                    Ok(response) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        status.set(format!("Create failed: HTTP {}", response.status()))
                    }
                    Err(_) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        status.set("Conversation service did not answer.".into());
                    }
                },
                Err(_) => {
                    if lifecycle_is_active(&lifecycle) {
                        status.set("Conversation request could not be encoded.".into());
                    }
                }
            }
        });
    };
    let send_lifecycle = Arc::clone(&lifecycle);
    let send_user_id = user_id.clone();
    let send = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        if submitting.get_untracked()
            || resolving_run.get_untracked()
            || active_run.get_untracked().is_some()
        {
            return;
        }
        let Some(conversation) = selected.get_untracked() else {
            return;
        };
        let draft_value = draft.get_untracked();
        let text = draft_value.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let existing_submission = pending_submission.get_untracked();
        let reusable_submission = existing_submission
            .filter(|pending| pending.conversation_id == conversation.id && pending.text == text);
        let client_submission_id = reusable_submission
            .as_ref()
            .map(|pending| pending.client_submission_id.clone())
            .or_else(new_client_submission_id);
        let Some(client_submission_id) = client_submission_id else {
            status.set("Secure browser randomness is unavailable; the turn was not sent.".into());
            return;
        };
        let model_id = reusable_submission.map_or_else(
            || {
                chat_models
                    .get_untracked()
                    .into_iter()
                    .find(|model| model.active)
                    .map(|model| model.id)
            },
            |pending| pending.model_id,
        );
        let submission = PendingSubmission {
            user_id: send_user_id.clone(),
            conversation_id: conversation.id.clone(),
            client_submission_id,
            text,
            model_id,
        };
        if store_pending_submission(&submission).is_err() {
            status.set(
                "This retry key could not be saved in the browser; the turn was not sent.".into(),
            );
            return;
        }
        pending_submission.set(Some(submission.clone()));
        submitting.set(true);
        let task_generation = advance_generation(generation);
        streamed.set(String::new());
        tool_calls.set(Vec::new());
        status.set("Indexing your message...".into());
        let lifecycle = Arc::clone(&send_lifecycle);
        spawn_local(async move {
            let turn_request = Request::post(&format!(
                "/api/v1/conversations/{}/turns",
                submission.conversation_id
            ))
            .json(&StartTurnRequest {
                client_submission_id: &submission.client_submission_id,
                text: &submission.text,
                model_id: submission.model_id.as_deref(),
            });
            let response = match turn_request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        let response = response.json::<TurnResponse>().await;
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        match response {
                            Ok(response) => Some(response),
                            Err(_) => {
                                if conversation_task_is_current(
                                    &lifecycle,
                                    generation,
                                    task_generation,
                                    selected,
                                    &submission.conversation_id,
                                ) {
                                    status.set(
                                            "Turn response was not valid. Retry will reuse this submission."
                                                .into(),
                                        );
                                    submitting.set(false);
                                }
                                None
                            }
                        }
                    }
                    Ok(response) => {
                        if conversation_task_is_current(
                            &lifecycle,
                            generation,
                            task_generation,
                            selected,
                            &submission.conversation_id,
                        ) {
                            status.set(format!(
                                "Turn rejected: HTTP {}. Retry will reuse this submission.",
                                response.status()
                            ));
                            submitting.set(false);
                        }
                        None
                    }
                    Err(_) => {
                        if conversation_task_is_current(
                            &lifecycle,
                            generation,
                            task_generation,
                            selected,
                            &submission.conversation_id,
                        ) {
                            status.set(
                                "Conversation service did not answer. Retry will reuse this submission."
                                    .into(),
                            );
                            submitting.set(false);
                        }
                        None
                    }
                },
                Err(_) => {
                    if conversation_task_is_current(
                        &lifecycle,
                        generation,
                        task_generation,
                        selected,
                        &submission.conversation_id,
                    ) {
                        status.set("Turn request could not be encoded.".into());
                        submitting.set(false);
                    }
                    None
                }
            };
            let Some(response) = response else { return };
            if !conversation_task_is_current(
                &lifecycle,
                generation,
                task_generation,
                selected,
                &submission.conversation_id,
            ) {
                return;
            }
            clear_pending_submission_if_matching(&submission);
            submitting.set(false);
            pending_submission.set(None);
            if draft.get_untracked().trim() == submission.text {
                draft.set(String::new());
            }
            messages.update(|current| {
                current.push(MessageSummary {
                    role: "user".into(),
                    text: submission.text,
                    provider: None,
                    model: None,
                });
            });
            let active_session = ActiveRunSession {
                user_id: submission.user_id,
                conversation_id: conversation.id,
                run_id: response.run.id,
            };
            active_run.set(Some(active_session.run_id.clone()));
            resolving_run.set(false);
            if store_active_run(&active_session).is_ok() {
                status.set("Model is responding...".into());
            } else {
                status.set("Model is responding; reload recovery is unavailable.".into());
            }
            follow_run(
                active_session,
                task_generation,
                ChatTaskSignals {
                    generation,
                    selected,
                    messages,
                    streamed,
                    tool_calls,
                    active_run,
                    resolving_run,
                    status,
                },
                lifecycle,
            );
        });
    };
    let cancel_lifecycle = Arc::clone(&lifecycle);
    let cancel = move |_| {
        let Some(run_id) = active_run.get_untracked() else {
            return;
        };
        let Some(conversation) = selected.get_untracked() else {
            return;
        };
        let task_generation = generation.get_untracked();
        status.set("Canceling run...".into());
        let lifecycle = Arc::clone(&cancel_lifecycle);
        spawn_local(async move {
            let result = Request::post(&format!("/api/v1/runs/{run_id}/cancel"))
                .send()
                .await;
            if !run_task_is_current(
                &lifecycle,
                generation,
                task_generation,
                selected,
                &conversation.id,
                active_run,
                &run_id,
            ) {
                return;
            }
            match result {
                Ok(response) if response.ok() => status.set("Cancellation requested.".into()),
                Ok(response) => status.set(format!("Cancel failed: HTTP {}", response.status())),
                Err(_) => status.set("Run service did not answer.".into()),
            }
        });
    };
    let open_lifecycle = Arc::clone(&lifecycle);
    let open_user_id = user_id.clone();
    let open_conversation = move |conversation: ConversationSummary| {
        let conversation_id = conversation.id.clone();
        let task_generation = advance_generation(generation);
        selected.set(Some(conversation));
        messages.set(Vec::new());
        streamed.set(String::new());
        tool_calls.set(Vec::new());
        active_run.set(None);
        resolving_run.set(true);
        submitting.set(false);
        status.set("Checking for an active run...".into());
        draft.set(
            pending_submission
                .get_untracked()
                .filter(|pending| pending.conversation_id == conversation_id)
                .map_or_else(String::new, |pending| pending.text),
        );
        load_messages(
            &conversation_id,
            task_generation,
            generation,
            selected,
            messages,
            status,
            Arc::clone(&open_lifecycle),
        );
        resolve_active_run(
            open_user_id.clone(),
            conversation_id,
            task_generation,
            ChatTaskSignals {
                generation,
                selected,
                messages,
                streamed,
                tool_calls,
                active_run,
                resolving_run,
                status,
            },
            Arc::clone(&open_lifecycle),
        );
    };
    let handle_composer_keydown = std::sync::Arc::new(move |event: web_sys::KeyboardEvent| {
        if event.key() == "Enter" && !event.shift_key() {
            event.prevent_default();
            if let Some(target) = event.target()
                && let Ok(el) = target.dyn_into::<web_sys::HtmlTextAreaElement>()
                && let Some(form) = el.form()
            {
                let _ = form.request_submit();
            }
        }
    });
    view! {
        <div class="page-heading">
            <div><p class="utility">"CONVERSATIONS / DURABLE"</p><h1>{move || selected.get().map_or_else(|| "What are we working on?".into(), |conversation| conversation.title)}</h1></div>
            <div class="header-chips">
                <span class="model-chip">{move || chat_models.get().into_iter().find(|model| model.active).map_or_else(|| "NO ACTIVE MODEL".into(), |model| format!("{} / {}", model.provider_type.to_uppercase(), model.model_reference))}</span>
                {ContextInspector(ContextInspectorProps { active_run })}
            </div>
        </div>
        <div class="workspace-picker" role="region" aria-label="Workspace scope">
            <label class="workspace-picker-label">
                <span class="utility">"WORKSPACE"</span>
                <select
                    aria-label="Workspace"
                    prop:value=move || selected_workspace.get().unwrap_or_default()
                    on:change=move |event| {
                        let value = event_target_value(&event);
                        if value.trim().is_empty() {
                            selected_workspace.set(None);
                        } else {
                            selected_workspace.set(Some(value));
                        }
                    }
                >
                    <option value="">"All workspaces"</option>
                    {move || workspaces.get().into_iter().map(|ws| {
                        let ws_id = ws.id.clone();
                        let ws_title = ws.title.clone();
                        view! { <option value=ws_id>{ws_title}</option> }
                    }).collect_view()}
                </select>
            </label>
            <span class="form-note">{move || {
                if let Some(id) = selected_workspace.get() {
                    if let Some(ws) = workspaces.get().into_iter().find(|w| w.id == id) {
                        format!("Chatting in {} — new conversations will be scoped here.", ws.title)
                    } else {
                        "Workspace scope active — new conversations scoped to selection.".into()
                    }
                } else {
                    "No workspace filter — conversations span all workspaces.".into()
                }
            }}</span>
        </div>

        {move || if let Some(conversation) = selected.get() {
            let conversation_id = conversation.id.clone();
            let send = send.clone();
            let cancel = cancel.clone();
            let handle_composer_keydown = handle_composer_keydown.clone();
            view! {
                <div class="conversation-toolbar">
                    <button class="text-button" on:click=move |_| {
                        if let Some(el) = transcript_ref.get() {
                            transcript_scroll.set(el.scroll_top());
                        }
                        advance_generation(generation);
                        selected.set(None);
                        streamed.set(String::new());
                        tool_calls.set(Vec::new());
                        active_run.set(None);
                        resolving_run.set(false);
                        submitting.set(false);
                    }>"← All conversations"</button>
                    <PinnedContext conversation_id=conversation_id.clone() />
                    <span class="utility">{format!("INDEX {}", &conversation_id[..8.min(conversation_id.len())])}</span>
                </div>
                <div class="transcript" node_ref=transcript_ref aria-live="polite"
                     on:scroll=move |_| {
                        if let Some(el) = transcript_ref.get() {
                            transcript_scroll.set(el.scroll_top());
                        }
                    }>
                    {move || messages.get().into_iter().map(|message| {
                        let provenance = match (&message.provider, &message.model) {
                            (Some(provider), Some(model)) => format!("{provider} / {model}"),
                            _ => "LOCAL USER".into(),
                        };
                        let html = markdown_to_html(&message.text);
                        let is_assistant = message.role == "assistant";
                        view! { <article class=if is_assistant { "message-entry assistant" } else { "message-entry user" }>
                            <div><span class="utility">{message.role.to_uppercase()}</span><small>{provenance}</small></div>
                            <div class="md-body" inner_html=html></div>
                        </article> }
                    }).collect_view()}
                    {move || {
                        let calls = tool_calls.get();
                        if calls.is_empty() {
                            None
                        } else {
                            Some(view! {
                                <div class="tool-drawer">
                                    {calls.into_iter().map(|call| {
                                        let call_id = call.id.clone();
                                        let name = call.name.clone();
                                        let args = call.args.clone();
                                        let result = call.result.clone().unwrap_or_default();
                                        let is_error = call.is_error;
                                        let expanded = RwSignal::new(!call.collapsed);
                                        view! {
                                            <details class="tool-card" open=move || expanded.get()>
                                                <summary on:click=move |e| { e.prevent_default(); expanded.update(|v| *v = !*v); }>
                                                    <span class="tool-name">{name.clone()}</span>
                                                    <span class="tool-id">{call_id.chars().take(8).collect::<String>()}</span>
                                                    <span class=if is_error { "tool-badge error" } else { "tool-badge ok" }>{if is_error { "ERROR" } else { "CALL" }}</span>
                                                    <span class="tool-toggle">{move || if expanded.get() { "▾" } else { "▸" }}</span>
                                                </summary>
                                                <div class="tool-section">
                                                    <p class="utility">"ARGS"</p>
                                                    <pre class="tool-pre">{args.clone()}</pre>
                                                </div>
                                                {if result.is_empty() {
                                                    view! { <div class="tool-section"><p class="utility">"AWAITING RESULT…"</p></div> }.into_any()
                                                } else {
                                                    view! { <div class="tool-section"><p class="utility">"RESULT"</p><pre class="tool-pre">{result}</pre></div> }.into_any()
                                                }}
                                            </details>
                                        }
                                    }).collect_view()}
                                </div>
                            })
                        }
                    }}
                    {move || (!streamed.get().is_empty()).then(|| {
                        let html = markdown_to_html(&streamed.get());
                        view! {
                        <article class="message-entry assistant streaming">
                            <div><span class="utility">"ASSISTANT"</span><small>"STREAMING / DURABLE"</small></div>
                            <div class="md-body" inner_html=html></div>
                            <span class="stream-caret"></span>
                        </article>
                    }})}
                    {move || (messages.get().is_empty() && tool_calls.get().is_empty() && active_run.get().is_none() && !resolving_run.get()).then(|| view! {
                        <div class="conversation-empty"><span class="index-spine">"NEW"</span><div><h2>"Start the durable record"</h2><p>"Messages, selected context, model events, and the final answer remain inspectable after this session closes."</p></div></div>
                    })}
                </div>
                <form class="composer" on:submit=send>
                    <textarea required rows="4" maxlength="1000000" placeholder="Write the next message — Enter to send, Shift+Enter for newline"
                        disabled=move || resolving_run.get() || active_run.get().is_some() || submitting.get()
                        prop:value=move || draft.get()
                        on:keydown=move |event| handle_composer_keydown(event)
                        on:input=move |event| {
                            let value = event_target_value(&event);
                            if let Some(pending) = pending_submission.get_untracked()
                                && value.trim() != pending.text
                            {
                                clear_pending_submission_if_matching(&pending);
                                pending_submission.set(None);
                            }
                            draft.set(value);
                        }></textarea>
                    <div><span>{move || status.get()}</span>
                        {move || if active_run.get().is_some() {
                            let cancel = cancel.clone();
                            view! { <button class="secondary" type="button" on:click=cancel>"Cancel run"</button> }.into_any()
                        } else if resolving_run.get() {
                            view! { <button class="secondary" type="button" disabled>"Checking run..."</button> }.into_any()
                        } else {
                            view! { <button class="primary" type="submit" disabled=move || submitting.get() || draft.get().trim().is_empty()>"Send + run"</button> }.into_any()
                        }}
                    </div>
                </form>
            }.into_any()
        } else {
            let create = create.clone();
            let open_conversation = open_conversation.clone();
            view! {
                <form class="filter-row" on:submit=create>
                    <input required maxlength="500" placeholder="Name a durable conversation"
                        prop:value=move || title.get()
                        on:input=move |event| title.set(event_target_value(&event)) />
                    <button class="primary" type="submit">"Create"</button>
                </form>
                <p class="form-note">{move || status.get()}</p>
                <div class="index-table" role="table">
                    <div class="index-row header" role="row"><span>"ID"</span><span>"TITLE"</span><span>"STATE"</span><span>"INDEX"</span></div>
                    {move || conversations.get().into_iter().map(|conversation| {
                        let short_id = conversation.id.chars().take(8).collect::<String>();
                        let selected_conversation = conversation.clone();
                        let open_conversation = open_conversation.clone();
                        view! { <button class="index-row index-action" type="button" role="row" on:click=move |_| {
                            open_conversation(selected_conversation.clone());
                        }>
                            <span class="spine-cell">{short_id}</span><strong>{conversation.title}</strong>
                            <span>{conversation.status}</span><span>"MESSAGE + BOOK"</span>
                        </button> }
                    }).collect_view()}
                </div>
            }.into_any()
        }}
    }
}

/// Context Inspector panel for displaying routing and token budget information.
#[component]
fn ContextInspector(active_run: RwSignal<Option<String>>) -> impl IntoView {
    let context_data = RwSignal::new(None::<ContextResponse>);
    let is_open = RwSignal::new(false);
    let active_tab = RwSignal::new(0u8);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    // Fetch context data when panel opens
    let fetch_context = move || {
        let run_id = match active_run.get() {
            Some(id) if !id.trim().is_empty() => id,
            _ => return,
        };
        loading.set(true);
        error.set(None);
        context_data.set(None);

        spawn_local(async move {
            match Request::get(&format!("/api/v1/runs/{}/context", run_id))
                .send()
                .await
            {
                Ok(response) if response.ok() => match response.json::<ContextResponse>().await {
                    Ok(data) => {
                        context_data.set(Some(data));
                        loading.set(false);
                    }
                    Err(e) => {
                        error.set(Some(format!("Failed to parse response: {}", e)));
                        loading.set(false);
                    }
                },
                Ok(response) => {
                    error.set(Some(format!("HTTP {}", response.status())));
                    loading.set(false);
                }
                Err(e) => {
                    error.set(Some(format!("Request failed: {}", e)));
                    loading.set(false);
                }
            }
        });
    };

    let toggle_panel = move |_| {
        let new_state = !is_open.get();
        is_open.set(new_state);
        if new_state && context_data.get().is_none() && active_run.get().is_some() {
            fetch_context();
        }
    };

    let set_tab = move |tab: u8| {
        active_tab.set(tab);
    };

    view! {
        <div class="context-inspector">
            // Toggle chip in header
            <button
                class="context-chip"
                on:click=toggle_panel
                title="Toggle context inspector"
            >
                {move || {
                    let run_id = active_run.get();
                    if run_id.is_none() {
                        "Context: —".into()
                    } else if let Some(data) = context_data.get() {
                        if let Some(budget) = &data.token_budget {
                            format!("Context: {}k / {}k", budget.total_used / 1000, budget.budget / 1000)
                        } else {
                            "Context: …".into()
                        }
                    } else {
                        "Context: …".into()
                    }
                }}
            </button>

            // Panel
            <div class="context-panel" class:open=move || is_open.get()>
                <div class="context-panel-header">
                    <h3>"Context Inspector"</h3>
                    <button class="text-button" on:click=toggle_panel>"✕"</button>
                </div>

                // Tab navigation
                <div class="context-tabs">
                    <button
                        class="context-tab"
                        class:active=move || active_tab.get() == 0
                        on:click=move |_| set_tab(0)
                    >"Budget"</button>
                    <button
                        class="context-tab"
                        class:active=move || active_tab.get() == 1
                        on:click=move |_| set_tab(1)
                    >"Why Loaded"</button>
                    <button
                        class="context-tab"
                        class:active=move || active_tab.get() == 2
                        on:click=move |_| set_tab(2)
                    >"Model"</button>
                </div>

                // Content area
                <div class="context-content">
                    // Loading state
                    {move || loading.get().then(|| view! {
                        <div class="context-empty">
                            <p>"Loading context data..."</p>
                        </div>
                    })}

                    // Error state
                    {move || error.get().map(|err| view! {
                        <div class="context-empty">
                            <p class="error-note">{err}</p>
                        </div>
                    })}

                    // No run active
                    {move || (!loading.get() && error.get().is_none() && active_run.get().is_none()).then(|| view! {
                        <div class="context-empty">
                            <p>"No active run. Start a conversation to inspect context."</p>
                        </div>
                    })}

                    // Pre-M23 runs
                    {move || {
                        let data = context_data.get();
                        match data {
                            Some(ctx) => {
                                if ctx.schema_version.is_some() && ctx.schema_version.unwrap_or(0) < 23 {
                                    return Some(view! {
                                        <div class="context-empty">
                                            <p>"Run predates adaptive routing (M22 or earlier)"</p>
                                        </div>
                                    }.into_any());
                                }
                                // Check if we should show budget tab content
                                if active_tab.get() == 0 && ctx.token_budget.is_some() {
                                    return None; // Let budget tab render
                                }
                                if active_tab.get() == 1 && ctx.routing_decisions.is_some() {
                                    return None; // Let why loaded tab render
                                }
                                if active_tab.get() == 2 && ctx.model_routing.is_some() {
                                    return None; // Let model tab render
                                }
                                // No data for current tab
                                    Some(view! {
                                        <div class="context-empty">
                                            <p>"No data available for this tab"</p>
                                        </div>
                                    }.into_any())
                            }
                            None => None,
                        }
                    }}

                    // Budget tab
                    {move || {
                        if active_tab.get() != 0 || loading.get() || error.get().is_some() || active_run.get().is_none() {
                            return None;
                        }
                        let data = context_data.get();
                        let budget = data.and_then(|d| d.token_budget);

                        if budget.is_none() {
                            return Some(view! {
                                <div class="context-empty">
                                    <p>"No budget data available"</p>
                                </div>
                            }.into_any());
                        }

                        let b = budget.unwrap();
                        Some(view! {
                            <div class="budget-tab">
                                <div class="budget-summary">
                                    <div class="budget-row">
                                        <span>"Used"</span>
                                        <strong>{format!("{} tokens", b.total_used)}</strong>
                                    </div>
                                    <div class="budget-row">
                                        <span>"Budget"</span>
                                        <strong>{format!("{} tokens", b.budget)}</strong>
                                    </div>
                                    <div class="budget-bar">
                                        <div
                                            class="budget-bar-fill"
                                            style=move || {
                                                let pct = if b.budget > 0 {
                                                    (b.total_used as f32 / b.budget as f32 * 100.0).min(100.0)
                                                } else { 0.0 };
                                                format!("width: {}%", pct)
                                            }
                                        ></div>
                                    </div>
                                </div>
                                <div class="budget-breakdown">
                                    <h4>"Token Usage by Category"</h4>
                                    <div class="budget-item">
                                        <span>"Conversation"</span>
                                        <span>{format!("{}", b.conversation)}</span>
                                    </div>
                                    <div class="budget-item">
                                        <span>"Source Books"</span>
                                        <span>{format!("{}", b.source_books)}</span>
                                    </div>
                                    <div class="budget-item">
                                        <span>"Skill Books"</span>
                                        <span>{format!("{}", b.skill_books)}</span>
                                    </div>
                                    <div class="budget-item">
                                        <span>"Plugin Books"</span>
                                        <span>{format!("{}", b.plugin_books)}</span>
                                    </div>
                                    <div class="budget-item">
                                        <span>"MCP Schemas"</span>
                                        <span>{format!("{}", b.mcp_schemas)}</span>
                                    </div>
                                    <div class="budget-item">
                                        <span>"Workspace"</span>
                                        <span>{format!("{}", b.workspace)}</span>
                                    </div>
                                    <div class="budget-item">
                                        <span>"System Policy"</span>
                                        <span>{format!("{}", b.system_policy)}</span>
                                    </div>
                                </div>
                            </div>
                        }.into_any())
                    }}

                    // Why Loaded tab
                    {move || {
                        if active_tab.get() != 1 || loading.get() || error.get().is_some() || active_run.get().is_none() {
                            return None;
                        }
                        let data = context_data.get();
                        let decisions = data.and_then(|d| d.routing_decisions);

                        if decisions.is_none() {
                            return Some(view! {
                                <div class="context-empty">
                                    <p>"No routing decisions available"</p>
                                </div>
                            }.into_any());
                        }

                        let decs = decisions.unwrap();
                        Some(view! {
                            <div class="why-loaded-tab">
                                <div class="routing-list">
                                    {decs.into_iter().map(|decision| {
                                        let action_class = if decision.action == "selected" { "selected" } else { "omitted" };
                                        let kind = decision.book_kind.clone();
                                        let kind2 = kind.clone();
                                        let kind3 = kind.clone();
                                        let kind4 = kind.clone();
                                        let title = decision.book_title.clone();
                                        let action = decision.action.clone();
                                        let reason = decision.reason.clone();
                                        view! {
                                            <div class="routing-item">
                                                <div class="routing-item-header">
                                                    <strong>{title}</strong>
                                                    <span class="kind-badge" class:source=move || kind == "source"
                                                          class:skill=move || kind2 == "skill"
                                                          class:plugin=move || kind3 == "plugin"
                                                          class:mcp=move || kind4 == "mcp">
                                                        {decision.book_kind}
                                                    </span>
                                                    <span class=format!("action-badge {}", action_class)>
                                                        {action}
                                                    </span>
                                                </div>
                                                <p class="routing-reason">{reason}</p>
                                            </div>
                                        }
                                    }).collect_view()}
                                </div>
                            </div>
                        }.into_any())
                    }}

                    // Model tab
                    {move || {
                        if active_tab.get() != 2 || loading.get() || error.get().is_some() || active_run.get().is_none() {
                            return None;
                        }
                        let data = context_data.get();
                        let model_info = data.and_then(|d| d.model_routing);

                        if model_info.is_none() {
                            return Some(view! {
                                <div class="context-empty">
                                    <p>"No model routing data available"</p>
                                </div>
                            }.into_any());
                        }

                        let info = model_info.unwrap();
                        Some(view! {
                            <div class="model-tab">
                                <div class="model-info">
                                    <div class="model-info-row">
                                        <span>"Selected Model"</span>
                                        <strong>{info.model_id}</strong>
                                    </div>
                                    <div class="model-routing-reason">
                                        <h4>"Routing Reason"</h4>
                                        <p>{info.routing_reason}</p>
                                    </div>
                                </div>
                            </div>
                        }.into_any())
                    }}
                </div>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Workspace + MCP page types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct WorkspaceSummary {
    id: String,
    title: String,
    description: String,
    network_policy: String,
    #[serde(default)]
    model_preference: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct CreateWorkspaceBody {
    title: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Serialize)]
struct CreateMcpServerBody {
    name: String,
    transport: String,
    configuration: serde_json::Value,
    enabled: bool,
}

/// Mirrors `skills_api.rs::CreateSkillRequest` (the server creates the
/// companion SKILL book in the same transaction).
#[derive(Debug, Serialize)]
struct CreateSkillBody {
    name: String,
    #[serde(default)]
    description: String,
    content: String,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    source_conversation_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    promotion_policy: Option<String>,
}

// ---------------------------------------------------------------------------
// Lane F: plugin install + detail types (mirror plugin_api.rs DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryFilter {
    All,
    Source,
    Skill,
    Plugin,
    Mcp,
}

impl LibraryFilter {
    fn label(self) -> &'static str {
        match self {
            LibraryFilter::All => "ALL",
            LibraryFilter::Source => "SOURCE",
            LibraryFilter::Skill => "SKILL",
            LibraryFilter::Plugin => "PLUGIN",
            LibraryFilter::Mcp => "MCP",
        }
    }

    /// Server-side kind filter for `GET /library/search` (None = all kinds).
    fn kind(self) -> Option<&'static str> {
        match self {
            LibraryFilter::All => None,
            LibraryFilter::Source => Some("SOURCE"),
            LibraryFilter::Skill => Some("SKILL"),
            LibraryFilter::Plugin => Some("PLUGIN"),
            LibraryFilter::Mcp => Some("MCP"),
        }
    }

    /// Client-side match for list responses (SOURCE also matches legacy
    /// NULL-kind books, mirroring the search semantics).
    fn matches(self, kind: &Option<String>) -> bool {
        match self {
            LibraryFilter::All => true,
            LibraryFilter::Source => matches!(kind.as_deref(), None | Some("SOURCE")),
            LibraryFilter::Skill => kind.as_deref() == Some("SKILL"),
            LibraryFilter::Plugin => kind.as_deref() == Some("PLUGIN"),
            LibraryFilter::Mcp => kind.as_deref() == Some("MCP"),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct PluginIdentity {
    source_type: String,
    source_uri: String,
    commit_sha: Option<String>,
    version: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct PluginSourceRef {
    source_type: String,
    source_uri: String,
    commit_sha: Option<String>,
    digest: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ComponentPreview {
    #[serde(rename = "type")]
    component_type: String,
    name: String,
    #[serde(rename = "ref")]
    component_ref: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PermissionPreview {
    domain: String,
    scope_value: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SelfTestPreview {
    command: Vec<String>,
    timeout: u32,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct PluginPreview {
    identity: PluginIdentity,
    name: String,
    version: String,
    publisher: String,
    description: String,
    source: PluginSourceRef,
    components: Vec<ComponentPreview>,
    permissions: Vec<PermissionPreview>,
    network_policy: String,
    #[serde(default)]
    resource_limits: Option<serde_json::Value>,
    #[serde(default)]
    self_test: Option<SelfTestPreview>,
    trust: String,
    #[serde(default)]
    update_policy: String,
}

#[derive(Debug, Clone, Deserialize)]
struct InstallResponse {
    plugin_id: String,
    book_id: String,
    state: String,
    trust: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ComponentRow {
    component_type: String,
    name: String,
    manifest_ref: String,
    metadata: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct InstallationRow {
    id: String,
    version: String,
    artifact_digest: String,
    status: String,
    installed_by: Option<String>,
    #[serde(default)]
    self_test_result: Option<serde_json::Value>,
    installed_at: Option<String>,
    activated_at: Option<String>,
    rolled_back_at: Option<String>,
    created_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct PluginDetail {
    id: String,
    name: String,
    description: String,
    version: String,
    publisher: Option<String>,
    source_type: String,
    source_uri: String,
    commit_sha: Option<String>,
    artifact_digest: Option<String>,
    #[serde(default)]
    signature: Option<serde_json::Value>,
    verified: bool,
    trust: String,
    state: String,
    install_path: Option<String>,
    manifest_version: i32,
    #[serde(default)]
    sandbox_policy: serde_json::Value,
    network_policy: String,
    #[serde(default)]
    resource_limits: Option<serde_json::Value>,
    workspace_id: Option<String>,
    created_at: String,
    updated_at: String,
    components: Vec<ComponentRow>,
    permissions: Vec<PermissionPreview>,
    installations: Vec<InstallationRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct DiffPair<T> {
    added: Vec<T>,
    removed: Vec<T>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct UpgradeDiff {
    installation_id: String,
    current_version: String,
    new_version: String,
    artifact_digest: String,
    commit_sha: Option<String>,
    permissions: DiffPair<PermissionPreview>,
    components: DiffPair<String>,
    capabilities: DiffPair<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct MarketplaceResult {
    name: String,
    publisher: String,
    version: String,
    description: String,
    source: String,
    source_uri: String,
    trust: String,
    #[serde(default)]
    capabilities: Vec<String>,
    popularity: u64,
}

#[derive(Debug, Serialize)]
struct PluginPreviewRequest<'a> {
    source_type: &'a str,
    source_uri: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct PluginInstallRequest<'a> {
    source_type: &'a str,
    source_uri: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    expected_digest: &'a str,
    approve: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct PluginSearchRequest<'a> {
    query: &'a str,
}

#[derive(Debug, Serialize)]
struct PatchPluginRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trust: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct PluginUpgradeRequest<'a> {
    version: &'a str,
}

/// Modal install stepper state (steps mirror the brief: source → preview →
/// approval → progress → done/error).
#[derive(Debug, Clone, PartialEq, Eq)]
enum StepperStep {
    Source,
    Previewing,
    Preview,
    Approve,
    Installing,
    Done,
    Error,
}

#[derive(Debug, Clone)]
struct InstallStepperState {
    step: StepperStep,
    source_type: String,
    source_uri: String,
    version: Option<String>,
    workspace_id: Option<String>,
    preview: Option<PluginPreview>,
    install: Option<InstallResponse>,
    confirmed: bool,
    phase: String,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Lane F: sandbox terminal + file manager types (mirror sandbox_api.rs DTOs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct TerminalStartBody<'a> {
    workspace_id: &'a str,
    command: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cols: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
struct TerminalStartResponse {
    terminal_id: String,
    network_policy: String,
}

#[derive(Debug, Serialize)]
struct TerminalWriteBody<'a> {
    workspace_id: &'a str,
    data_base64: &'a str,
}

#[derive(Debug, Serialize)]
struct TerminalReadBody<'a> {
    workspace_id: &'a str,
    after_cursor: u64,
    max_bytes: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct TerminalReadResponse {
    data_base64: String,
    next_cursor: u64,
    state: String,
}

#[derive(Debug, Serialize)]
struct TerminalResizeBody<'a> {
    workspace_id: &'a str,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Serialize)]
struct WorkspaceIdBody<'a> {
    workspace_id: &'a str,
}

#[derive(Debug, Serialize)]
struct SandboxPathBody<'a> {
    workspace_id: &'a str,
    path: &'a str,
}

#[derive(Debug, Serialize)]
struct SandboxWriteBody<'a> {
    workspace_id: &'a str,
    path: &'a str,
    data_base64: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct FileListResponse {
    entries: Vec<FsEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct FsEntry {
    name: String,
    kind: String,
    size: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct ReadFileResponse {
    data_base64: String,
    sha256: String,
}

/// A persisted browser terminal session (survives page reloads so the user
/// can reconnect to a fresh session in the same workspace).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct OpenTerminalSession {
    workspace_id: String,
    terminal_id: String,
}

/// Mirrors the active chat conversation so the unified Library page can offer
/// "pin to current conversation" without ChatPage exposing its internals.
fn active_conversation() -> RwSignal<Option<ConversationSummary>> {
    static ACTIVE_CONVERSATION: std::sync::OnceLock<RwSignal<Option<ConversationSummary>>> =
        std::sync::OnceLock::new();
    ACTIVE_CONVERSATION
        .get_or_init(|| RwSignal::new(None))
        .to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolCallCard {
    id: String,
    name: String,
    args: String,
    result: Option<String>,
    is_error: bool,
    collapsed: bool,
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn inline_code_html(escaped: &str) -> String {
    format!(
        "<code style=\"background:var(--hair);padding:1px 5px;border-radius:5px;font-family:var(--mono);font-size:12.5px;border:1px solid var(--line);\">{escaped}</code>"
    )
}

fn render_inline(text: &str) -> String {
    let mut out = escape_html(text);
    let mut result = String::new();
    let mut rest = out.as_str();
    while let Some(start) = rest.find('`') {
        if let Some(end) = rest[start + 1..].find('`') {
            result.push_str(&rest[..start]);
            let code = &rest[start + 1..start + 1 + end];
            result.push_str(&inline_code_html(code));
            rest = &rest[start + 1 + end + 1..];
        } else {
            break;
        }
    }
    result.push_str(rest);
    out = result;
    fn replace_delim(input: &str, delim: &str, open: &str, close: &str) -> String {
        let mut res = String::new();
        let mut remaining = input;
        while let Some(start) = remaining.find(delim) {
            if let Some(end) = remaining[start + delim.len()..].find(delim) {
                res.push_str(&remaining[..start]);
                let inner = &remaining[start + delim.len()..start + delim.len() + end];
                res.push_str(open);
                res.push_str(inner);
                res.push_str(close);
                remaining = &remaining[start + delim.len() + end + delim.len()..];
            } else {
                break;
            }
        }
        res.push_str(remaining);
        res
    }
    out = replace_delim(
        &out,
        "**",
        "<strong style=\"font-weight:700;\">",
        "</strong>",
    );
    out = replace_delim(
        &out,
        "__",
        "<strong style=\"font-weight:700;\">",
        "</strong>",
    );
    out = replace_delim(&out, "*", "<em style=\"font-style:italic;\">", "</em>");
    out = replace_delim(&out, "_", "<em style=\"font-style:italic;\">", "</em>");
    let mut linked = String::new();
    let mut r = out.as_str();
    while let Some(lb) = r.find('[') {
        if let Some(rb) = r[lb..].find(']') {
            let after = lb + rb;
            if r[after + 1..].starts_with('(') {
                if let Some(paren_end) = r[after + 1..].find(')') {
                    let text = &r[lb + 1..lb + rb];
                    let url = &r[after + 2..after + 1 + paren_end];
                    let safe_url = if url.starts_with("http://")
                        || url.starts_with("https://")
                        || url.starts_with('/')
                        || url.starts_with('#')
                    {
                        escape_html(url)
                    } else {
                        continue;
                    };
                    linked.push_str(&r[..lb]);
                    linked.push_str(&format!("<a href=\"{safe_url}\" target=\"_blank\" rel=\"noopener noreferrer\" style=\"color:var(--indigo);text-decoration:underline;\">{text}</a>"));
                    r = &r[after + 1 + paren_end + 1..];
                    continue;
                }
            }
        }
        break;
    }
    linked.push_str(r);
    if !linked.is_empty() && linked != out {
        out = linked;
    }
    out
}

fn markdown_to_html(input: &str) -> String {
    if input.trim().is_empty() {
        return String::new();
    }
    let mut html = String::new();
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut list_buffer: Vec<String> = Vec::new();
    let mut in_list = false;
    let mut blockquote_buf: Vec<String> = Vec::new();
    let mut in_blockquote = false;

    let flush_list = |html: &mut String, buf: &mut Vec<String>| {
        if buf.is_empty() {
            return;
        }
        html.push_str("<ul style=\"margin:6px 0 10px 18px;padding:0;list-style:disc;\">");
        for li in buf.drain(..) {
            html.push_str(&format!(
                "<li style=\"margin:2px 0;line-height:1.55;\">{}</li>",
                render_inline(&li)
            ));
        }
        html.push_str("</ul>");
    };
    let flush_blockquote = |html: &mut String, buf: &mut Vec<String>| {
        if buf.is_empty() {
            return;
        }
        html.push_str("<blockquote style=\"margin:8px 0;padding:8px 12px;border-left:3px solid var(--indigo);background:rgba(44,62,204,0.06);border-radius:0 8px 8px 0;\">");
        for line in buf.drain(..) {
            html.push_str(&format!(
                "<p style=\"margin:0;\">{}</p>",
                render_inline(&line)
            ));
        }
        html.push_str("</blockquote>");
    };

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        if trimmed.starts_with("```") {
            if in_code_block {
                let lang_label = if !code_lang.is_empty() {
                    format!(
                        "<span style=\"position:absolute;top:6px;right:8px;font:600 10px var(--mono);letter-spacing:0.06em;color:var(--slate);text-transform:uppercase;\">{}</span>",
                        escape_html(&code_lang)
                    )
                } else {
                    String::new()
                };
                html.push_str(&format!("<div style=\"position:relative;margin:10px 0;overflow:hidden;border:1px solid var(--line);border-radius:10px;background:#0f1a1e;\">{lang_label}<pre style=\"margin:0;padding:14px 14px;overflow:auto;font:12.5px var(--mono);line-height:1.6;color:#e6f0ee;white-space:pre;word-wrap:normal;\"><code>{}</code></pre></div>", escape_html(&code_buf)));
                code_buf.clear();
                code_lang.clear();
                in_code_block = false;
            } else {
                flush_list(&mut html, &mut list_buffer);
                in_list = false;
                flush_blockquote(&mut html, &mut blockquote_buf);
                in_blockquote = false;
                in_code_block = true;
                code_lang = trimmed.trim_start_matches('`').trim().to_owned();
            }
            i += 1;
            continue;
        }
        if in_code_block {
            code_buf.push_str(raw);
            code_buf.push('\n');
            i += 1;
            continue;
        }
        if trimmed.is_empty() {
            flush_list(&mut html, &mut list_buffer);
            in_list = false;
            flush_blockquote(&mut html, &mut blockquote_buf);
            in_blockquote = false;
            i += 1;
            continue;
        }
        if trimmed.starts_with("> ") || trimmed == ">" {
            if in_list {
                flush_list(&mut html, &mut list_buffer);
                in_list = false;
            }
            in_blockquote = true;
            let content = if trimmed.len() > 2 {
                trimmed[2..].to_owned()
            } else {
                String::new()
            };
            blockquote_buf.push(content);
            i += 1;
            continue;
        } else if in_blockquote {
            flush_blockquote(&mut html, &mut blockquote_buf);
            in_blockquote = false;
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            in_list = true;
            let content = trimmed[2..].to_owned();
            list_buffer.push(content);
            i += 1;
            continue;
        }
        let is_ordered = {
            let mut chars = trimmed.chars();
            let mut digits = 0;
            while let Some(ch) = chars.next() {
                if ch.is_ascii_digit() {
                    digits += 1;
                } else if ch == '.' && digits > 0 {
                    if chars.next() == Some(' ') {
                        break;
                    } else {
                        digits = 0;
                        break;
                    }
                } else {
                    digits = 0;
                    break;
                }
            }
            digits > 0 && trimmed.chars().skip_while(|c| c.is_ascii_digit()).next() == Some('.')
        };
        if is_ordered {
            in_list = true;
            let dot = trimmed.find('.').unwrap();
            let content = trimmed[dot + 2..].to_owned();
            list_buffer.push(content);
            i += 1;
            continue;
        }
        if in_list {
            flush_list(&mut html, &mut list_buffer);
            in_list = false;
        }
        if trimmed.starts_with("### ") {
            html.push_str(&format!("<h4 style=\"margin:10px 0 6px;font-family:var(--display);font-size:15px;font-weight:600;letter-spacing:-0.01em;\">{}</h4>", render_inline(trimmed[4..].trim())));
        } else if trimmed.starts_with("## ") {
            html.push_str(&format!("<h3 style=\"margin:12px 0 6px;font-family:var(--display);font-size:16px;font-weight:600;letter-spacing:-0.015em;\">{}</h3>", render_inline(trimmed[3..].trim())));
        } else if trimmed.starts_with("# ") {
            html.push_str(&format!("<h2 style=\"margin:14px 0 8px;font-family:var(--display);font-size:18px;font-weight:600;letter-spacing:-0.02em;\">{}</h2>", render_inline(trimmed[2..].trim())));
        } else if trimmed.starts_with("---") || trimmed.starts_with("***") {
            html.push_str(
                "<hr style=\"margin:12px 0;border:0;border-top:1px solid var(--hair);\" />",
            );
        } else {
            html.push_str(&format!(
                "<p style=\"margin:6px 0;line-height:1.65;\">{}</p>",
                render_inline(trimmed)
            ));
        }
        i += 1;
    }
    flush_list(&mut html, &mut list_buffer);
    flush_blockquote(&mut html, &mut blockquote_buf);
    if in_code_block {
        html.push_str(&format!("<div style=\"margin:10px 0;overflow:hidden;border:1px solid var(--line);border-radius:10px;background:#0f1a1e;\"><pre style=\"margin:0;padding:14px 14px;overflow:auto;font:12.5px var(--mono);line-height:1.6;color:#e6f0ee;\"><code>{}</code></pre></div>", escape_html(&code_buf)));
    }
    html
}

#[component]
fn WorkspacesPage() -> impl IntoView {
    let workspaces = RwSignal::new(Vec::<WorkspaceSummary>::new());
    let status = RwSignal::new(String::new());
    let title = RwSignal::new(String::new());
    let description = RwSignal::new(String::new());

    let load = move || {
        status.set("Loading workspaces...".into());
        spawn_local(async move {
            match Request::get("/api/v1/workspaces").send().await {
                Ok(response) if response.ok() => {
                    match response.json::<Vec<WorkspaceSummary>>().await {
                        Ok(list) => {
                            workspaces.set(list);
                            status.set(String::new());
                        }
                        Err(_) => status.set("Workspace response was not valid.".into()),
                    }
                }
                Ok(response) => status.set(format!(
                    "Workspace request failed: HTTP {}",
                    response.status()
                )),
                Err(_) => status.set("Workspace service did not answer.".into()),
            }
        });
    };

    load();

    let create = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let title_val = title.get_untracked().trim().to_owned();
        if title_val.is_empty() {
            status.set("Title is required.".into());
            return;
        }
        let desc_val = description.get_untracked().trim().to_owned();
        status.set("Creating workspace...".into());
        spawn_local(async move {
            let body = CreateWorkspaceBody {
                title: title_val,
                description: desc_val,
            };
            match Request::post("/api/v1/workspaces").json(&body) {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        title.set(String::new());
                        description.set(String::new());
                        load();
                    }
                    Ok(response) => {
                        status.set(format!("Create failed: HTTP {}", response.status()))
                    }
                    Err(_) => status.set("Workspace service did not answer.".into()),
                },
                Err(_) => status.set("Create request could not be encoded.".into()),
            }
        });
    };

    // Workspace editing: PATCH /api/v1/workspaces/{id}
    let editing = RwSignal::new(None::<WorkspaceSummary>);
    let edit_title = RwSignal::new(String::new());
    let edit_description = RwSignal::new(String::new());
    let edit_policy = RwSignal::new(String::new());
    let edit_model = RwSignal::new(String::new());
    let edit_status = RwSignal::new(String::new());
    let open_edit = move |ws: WorkspaceSummary| {
        edit_title.set(ws.title.clone());
        edit_description.set(ws.description.clone());
        edit_policy.set(ws.network_policy.clone());
        edit_model.set(ws.model_preference.clone().unwrap_or_default());
        edit_status.set(String::new());
        editing.set(Some(ws));
    };
    let save_edit = move |_| {
        let Some(ws) = editing.get_untracked() else {
            return;
        };
        let payload = serde_json::json!({
            "title": edit_title.get_untracked(),
            "description": edit_description.get_untracked(),
            "network_policy": edit_policy.get_untracked(),
            "model_preference": if edit_model.get_untracked().trim().is_empty() { None } else { Some(edit_model.get_untracked()) },
        });
        let id = ws.id.clone();
        spawn_local(async move {
            let request = gloo_net::http::Request::patch(&format!("/api/v1/workspaces/{id}"))
                .header("Content-Type", "application/json")
                .json(&payload);
            match request {
                Ok(request) => match request.send().await {
                    Ok(resp) if resp.ok() => {
                        // refresh the list
                        if let Ok(resp) = gloo_net::http::Request::get("/api/v1/workspaces")
                            .send()
                            .await
                        {
                            if let Ok(list) = resp.json::<Vec<WorkspaceSummary>>().await {
                                workspaces.set(list);
                            }
                        }
                        edit_status.set("Saved.".into());
                        editing.set(None);
                    }
                    Ok(resp) => {
                        let text = resp.text().await.unwrap_or_default();
                        let detail = serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|v| v["detail"].as_str().map(|s| s.to_string()))
                            .unwrap_or_else(|| "Update failed.".into());
                        edit_status.set(detail);
                    }
                    Err(_) => edit_status.set("Could not reach server.".into()),
                },
                Err(_) => edit_status.set("Request could not be encoded.".into()),
            }
        });
    };
    let close_edit = move |_| editing.set(None);
    let delete_target = RwSignal::new(None::<DeleteTarget>);
    let do_delete_workspace = {
        let workspaces = workspaces;
        let status = status;
        std::sync::Arc::new(move |id: String| {
            spawn_local(async move {
                let resp = Request::delete(&format!("/api/v1/workspaces/{id}"))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status() == 204 => {
                        status.set("Workspace deleted.".into());
                        if let Ok(list_resp) = Request::get("/api/v1/workspaces").send().await
                            && let Ok(list) = list_resp.json::<Vec<WorkspaceSummary>>().await
                        {
                            workspaces.set(list);
                        }
                    }
                    Ok(r) if r.status() == 409 => {
                        let msg = api_error(&r).await;
                        status.set(format!("Cannot delete workspace: {msg}"));
                    }
                    Ok(r) if r.status() == 403 => {
                        status.set("Owner/admin role required to delete workspaces.".into());
                    }
                    Ok(r) => {
                        let msg = api_error(&r).await;
                        if msg.to_lowercase().contains("forbidden")
                            || msg.to_lowercase().contains("admin")
                            || msg.to_lowercase().contains("owner")
                        {
                            status.set("Owner/admin role required to delete workspaces.".into());
                        } else {
                            status.set(format!("Delete failed: {msg}"));
                        }
                    }
                    Err(_) => status.set("Workspace service did not answer.".into()),
                }
            });
        })
    };

    view! {
        <div class="page-heading">
            <div><p class="utility">"GLOBAL CONTEXT / WORKSPACES"</p><h1>"Workspaces"</h1></div>
            <span class="utility">{move || format!("{} WORKSPACES", workspaces.get().len())}</span>
        </div>
        <form class="filter-row" on:submit=create>
            <input placeholder="Title" prop:value=move || title.get()
                on:input=move |event| title.set(event_target_value(&event)) />
            <input placeholder="Description (optional)" prop:value=move || description.get()
                on:input=move |event| description.set(event_target_value(&event)) />
            <button type="submit">"Create"</button>
        </form>
        <p class="form-note">{move || status.get()}</p>
        <div class="index-table" role="table">
            <div class="index-row header" role="row"><span>"ID"</span><span>"TITLE"</span><span>"DESCRIPTION"</span><span>"NETWORK POLICY"</span><span></span></div>
            {move || workspaces.get().into_iter().map(|ws| {
                let ws_clone = ws.clone();
                let ws_label = ws.title.clone();
                let ws_id = ws.id.clone();
                let short_id = ws.id.chars().take(8).collect::<String>();
                view! { <div class="index-row" role="row">
                    <span class="spine-cell">{short_id}</span>
                    <strong>{ws.title.clone()}</strong>
                    <span>{ws.description.clone()}</span>
                    <span>{ws.network_policy.to_uppercase()}</span>
                    <div style="display:flex;gap:6px;justify-content:flex-end">
                        <button type="button" class="icon-button" aria-label="Edit workspace"
                            on:click=move |_| open_edit(ws_clone.clone())>"✎"</button>
                        <button type="button" class="icon-button danger" title="Delete workspace" aria-label=format!("Delete workspace {}", ws_label.clone())
                            on:click=move |_| delete_target.set(Some(DeleteTarget::Workspace { id: ws_id.clone(), label: ws_label.clone() }))>"🗑"</button>
                    </div>
                </div> }
            }).collect_view()}
        </div>
        // Edit modal
        {move || editing.get().map(|_ws| {
            view! {
                <div class="modal-backdrop" on:click=move |_| editing.set(None)>
                    <div class="modal" on:click=|ev| ev.stop_propagation()>
                        <div class="modal-head"><h2>"Edit workspace"</h2><button class="icon-button" on:click=close_edit>"✕"</button></div>
                        <div class="modal-body">
                            <label>"Title"</label>
                            <input prop:value=move || edit_title.get()
                                on:input=move |event| edit_title.set(event_target_value(&event)) />
                            <label>"Description"</label>
                            <textarea rows="2" prop:value=move || edit_description.get()
                                on:input=move |event| edit_description.set(event_target_value(&event))></textarea>
                            <label>"Network policy"</label>
                            <select prop:value=move || edit_policy.get()
                                on:change=move |event| edit_policy.set(event_target_value(&event))>
                                <option value="NONE">"NONE — no egress"</option>
                                <option value="RESTRICTED">"RESTRICTED — allowlisted hosts"</option>
                                <option value="FULL">"FULL — unrestricted"</option>
                            </select>
                            <p class="form-note">"Controls sandbox/terminal network access for this workspace."</p>
                            <label>"Model preference (optional)"</label>
                            <input placeholder="model-reference" prop:value=move || edit_model.get()
                                on:input=move |event| edit_model.set(event_target_value(&event)) />
                            <p class="form-note">{move || edit_status.get()}</p>
                        </div>
                        <div class="modal-foot">
                            <button type="button" class="secondary" on:click=close_edit>"Cancel"</button>
                            <button type="button" class="primary" on:click=save_edit>"Save"</button>
                        </div>
                    </div>
                </div>
            }
        })}
        {move || {
            let dp = std::sync::Arc::clone(&do_delete_workspace);
            delete_target.get().clone().map(|target| {
                let (title, body) = match target.clone() {
                    DeleteTarget::Workspace { id, label } => (format!("Delete workspace {label}?"), format!("This will permanently delete the workspace. If it has worktrees or conversations it will be refused (409). Id: {}", &id[..8.min(id.len())])),
                    _ => (String::new(), String::new()),
                };
                let confirm_target = target.clone();
                let dp_inner = std::sync::Arc::clone(&dp);
                view! {
                    <div class="modal-backdrop" role="dialog" aria-modal="true">
                        <div class="modal delete-confirm">
                            <div class="modal-head"><h3>{title.clone()}</h3><button class="text-button" on:click=move |_| delete_target.set(None)>"✕"</button></div>
                            <p class="form-note">{body.clone()}</p>
                            <p class="error-note" style="font-size:13px">"This cannot be undone."</p>
                            <div style="display:flex;gap:10px;justify-content:flex-end;margin-top:12px">
                                <button class="secondary" on:click=move |_| delete_target.set(None)>"Cancel"</button>
                                <button class="primary danger" on:click=move |_| {
                                    if let DeleteTarget::Workspace { id, .. } = confirm_target.clone() { dp_inner(id); }
                                    delete_target.set(None);
                                }>"Delete"</button>
                            </div>
                        </div>
                    </div>
                }
            })
        }}
    }
}

#[component]
fn LibraryPage(initial_kind: Option<&'static str>, page: RwSignal<Page>) -> impl IntoView {
    let filter = RwSignal::new(match initial_kind {
        Some("SKILL") => LibraryFilter::Skill,
        Some("MCP") => LibraryFilter::Mcp,
        Some("PLUGIN") => LibraryFilter::Plugin,
        Some("SOURCE") => LibraryFilter::Source,
        _ => LibraryFilter::All,
    });
    let all_books = RwSignal::new(Vec::<BookSummary>::new());
    let search_hits = RwSignal::new(None::<Vec<BookSummary>>);
    let query = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let workspaces = RwSignal::new(Vec::<WorkspaceSummary>::new());
    let loaded = RwSignal::new(None::<LoadedBook>);
    let load_status = RwSignal::new(String::new());
    let plugin_detail_id = RwSignal::new(None::<String>);
    let plugin_detail_tick = RwSignal::new(0_u64);
    let marketplace_open = RwSignal::new(false);
    let marketplace_results = RwSignal::new(Vec::<MarketplaceResult>::new());
    let marketplace_query = RwSignal::new(String::new());
    let marketplace_status = RwSignal::new(String::new());
    let marketplace_loading = RwSignal::new(false);
    let new_menu = RwSignal::new(false);
    let new_form = RwSignal::new(None::<&'static str>);
    let create_title = RwSignal::new(String::new());
    let create_body = RwSignal::new(String::new());
    let create_tags = RwSignal::new(String::new());
    let create_scope = RwSignal::new("PROFILE".to_owned());
    let create_status = RwSignal::new(String::new());
    let creating = RwSignal::new(false);
    let skill_name = RwSignal::new(String::new());
    let skill_description = RwSignal::new(String::new());
    let skill_content = RwSignal::new(String::new());
    let skill_status = RwSignal::new(String::new());
    let skill_creating = RwSignal::new(false);
    let mcp_name = RwSignal::new(String::new());
    let mcp_transport = RwSignal::new("stdio".to_owned());
    let mcp_configuration = RwSignal::new(String::new());
    let mcp_status = RwSignal::new(String::new());
    let mcp_creating = RwSignal::new(false);
    let stepper = RwSignal::new(None::<InstallStepperState>);
    let stepper_source = RwSignal::new(String::new());
    let stepper_version = RwSignal::new(String::new());
    let stepper_workspace = RwSignal::new(String::new());
    // Edit/Delete surfaces for Library/MCP/Plugin books (task 6)
    let edit_book = RwSignal::new(None::<LoadedBook>);
    let edit_title = RwSignal::new(String::new());
    let edit_body = RwSignal::new(String::new());
    let edit_tags = RwSignal::new(String::new());
    let edit_reason = RwSignal::new(String::new());
    let edit_status = RwSignal::new(String::new());
    let edit_saving = RwSignal::new(false);
    let mcp_edit_id = RwSignal::new(None::<String>);
    let mcp_edit_name = RwSignal::new(String::new());
    let mcp_edit_config = RwSignal::new(String::new());
    let mcp_edit_enabled = RwSignal::new(true);
    let mcp_edit_status = RwSignal::new(String::new());
    let mcp_edit_saving = RwSignal::new(false);
    load_library_list(all_books, status, None);
    load_workspaces(workspaces);

    // ---- list refresh helpers ----
    let refresh = {
        move || {
            load_library_list(all_books, status, None);
            search_hits.set(None);
        }
    };

    // ---- search + filter ----
    let run_search = {
        move |event: leptos::ev::SubmitEvent| {
            event.prevent_default();
            let value = query.get_untracked();
            let trimmed = value.trim().to_owned();
            if trimmed.is_empty() {
                search_hits.set(None);
                load_library_list(all_books, status, None);
                return;
            }
            search_hits.set(Some(Vec::new()));
            status.set("Searching the index...".into());
            let kind = filter.get_untracked().kind().map(str::to_owned);
            spawn_local(async move {
                match search_library(&trimmed, kind.as_deref()).await {
                    Ok(found) => {
                        search_hits.set(Some(found));
                        status.set("Search complete.".into());
                    }
                    Err(error) => status.set(format!("Search failed: {error}")),
                }
            });
        }
    };
    let set_filter = {
        move |next: LibraryFilter| {
            filter.set(next);
            let value = query.get_untracked();
            if value.trim().is_empty() {
                search_hits.set(None);
                load_library_list(all_books, status, None);
            } else {
                let trimmed = value.trim().to_owned();
                let kind = next.kind().map(str::to_owned);
                spawn_local(async move {
                    match search_library(&trimmed, kind.as_deref()).await {
                        Ok(found) => search_hits.set(Some(found)),
                        Err(error) => status.set(format!("Search failed: {error}")),
                    }
                });
            }
        }
    };

    // ---- open kind-specific detail (progressive load) ----
    let open_book = {
        move |book_id: String| {
            load_status.set("Loading book content...".into());
            let loaded = loaded;
            let load_status = load_status;
            spawn_local(async move {
                match load_library_book(&book_id).await {
                    Ok(found) => {
                        loaded.set(Some(found));
                        load_status.set(String::new());
                    }
                    Err(error) => load_status.set(format!("Could not open book: {error}")),
                }
            });
        }
    };

    // ---- pin to the current conversation (when one is open) ----
    let pin_book = move |book_id: String| {
        let Some(conversation) = active_conversation().get_untracked() else {
            return;
        };
        let conversation_id = conversation.id.clone();
        status.set("Pinning to the current conversation...".into());
        spawn_local(async move {
            match Request::put(&format!(
                "/api/v1/conversations/{conversation_id}/pins/{book_id}"
            ))
            .send()
            .await
            {
                Ok(response) if response.ok() => {
                    status.set("Pinned to the current conversation.".into());
                }
                Ok(response) => status.set(format!("Pin failed: {}", api_error(&response).await)),
                Err(_) => status.set("Library service did not answer.".into()),
            }
        });
    };

    // ---- create: source book / skill / mcp server ----
    let create_book = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        if creating.get_untracked() {
            return;
        }
        let title = create_title.get_untracked();
        if title.trim().is_empty() {
            create_status.set("Title is required.".into());
            return;
        }
        let body = create_body.get_untracked();
        let tags = create_tags
            .get_untracked()
            .split(',')
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();
        let scope = create_scope.get_untracked();
        creating.set(true);
        create_status.set("Creating source book...".into());
        let all_books = all_books;
        let status = status;
        let loaded = loaded;
        let new_form = new_form;
        let create_title = create_title;
        let create_body = create_body;
        let create_tags = create_tags;
        let create_status = create_status;
        let creating = creating;
        spawn_local(async move {
            let request = Request::post("/api/v1/library/books").json(&CreateBookBody {
                title: title.trim().to_owned(),
                body,
                book_type: "NOTE".into(),
                scope,
                tags,
                provenance: "USER".into(),
                trust: "USER_PROVIDED".into(),
                workspace_id: None,
                conversation_id: None,
                security_classification: "INTERNAL".into(),
                metadata: serde_json::Value::Object(Default::default()),
            });
            let outcome = match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        create_title.set(String::new());
                        create_body.set(String::new());
                        create_tags.set(String::new());
                        new_form.set(None);
                        loaded.set(None);
                        "Source book created.".to_owned()
                    }
                    Ok(response) => format!("Create rejected: {}", api_error(&response).await),
                    Err(_) => "Library service did not answer.".to_owned(),
                },
                Err(_) => "Create request could not be encoded.".to_owned(),
            };
            create_status.set(outcome);
            creating.set(false);
            load_library_list(all_books, status, None);
        });
    };
    let create_skill = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        if skill_creating.get_untracked() {
            return;
        }
        let name = skill_name.get_untracked();
        if name.trim().is_empty() {
            skill_status.set("Name is required.".into());
            return;
        }
        let description = skill_description.get_untracked();
        let content = skill_content.get_untracked();
        skill_creating.set(true);
        skill_status.set("Creating skill...".into());
        let all_books = all_books;
        let status = status;
        let loaded = loaded;
        let new_form = new_form;
        let skill_name = skill_name;
        let skill_description = skill_description;
        let skill_content = skill_content;
        let skill_status = skill_status;
        let skill_creating = skill_creating;
        spawn_local(async move {
            let body = CreateSkillBody {
                name: name.trim().to_owned(),
                description,
                content,
                reason: "Created from the Library page".into(),
                workspace_id: None,
                source_conversation_ids: Vec::new(),
                promotion_policy: None,
            };
            let request = Request::post("/api/v1/skills").json(&body);
            let outcome = match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        skill_name.set(String::new());
                        skill_description.set(String::new());
                        skill_content.set(String::new());
                        new_form.set(None);
                        loaded.set(None);
                        "Skill created; its SKILL book is now indexed.".to_owned()
                    }
                    Ok(response) => format!("Create rejected: {}", api_error(&response).await),
                    Err(_) => "Skills service did not answer.".to_owned(),
                },
                Err(_) => "Create request could not be encoded.".to_owned(),
            };
            skill_status.set(outcome);
            skill_creating.set(false);
            load_library_list(all_books, status, None);
        });
    };
    let create_mcp = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        if mcp_creating.get_untracked() {
            return;
        }
        let name = mcp_name.get_untracked();
        if name.trim().is_empty() {
            mcp_status.set("Name is required.".into());
            return;
        }
        let transport = mcp_transport.get_untracked();
        let config_text = mcp_configuration.get_untracked().trim().to_owned();
        let config_value = if config_text.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str(&config_text) {
                Ok(value) => value,
                Err(error) => {
                    mcp_status.set(format!("Invalid configuration JSON: {error}"));
                    return;
                }
            }
        };
        mcp_creating.set(true);
        mcp_status.set("Creating MCP server...".into());
        let all_books = all_books;
        let status = status;
        let loaded = loaded;
        let new_form = new_form;
        let mcp_name = mcp_name;
        let mcp_configuration = mcp_configuration;
        let mcp_status = mcp_status;
        let mcp_creating = mcp_creating;
        spawn_local(async move {
            let body = CreateMcpServerBody {
                name: name.trim().to_owned(),
                transport,
                configuration: config_value,
                enabled: true,
            };
            let request = Request::post("/api/v1/mcp/servers").json(&body);
            let outcome = match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        mcp_name.set(String::new());
                        mcp_configuration.set(String::new());
                        new_form.set(None);
                        loaded.set(None);
                        "MCP server created; its MCP book is now indexed.".to_owned()
                    }
                    Ok(response) => format!("Create rejected: {}", api_error(&response).await),
                    Err(_) => "MCP server service did not answer.".to_owned(),
                },
                Err(_) => "Create request could not be encoded.".to_owned(),
            };
            mcp_status.set(outcome);
            mcp_creating.set(false);
            load_library_list(all_books, status, None);
        });
    };

    // ---- edit surfaces for Library / MCP (book PUT, MCP PATCH/DELETE) ----
    let open_book_edit = {
        let edit_book = edit_book;
        let edit_title = edit_title;
        let edit_body = edit_body;
        let edit_tags = edit_tags;
        let edit_reason = edit_reason;
        let edit_status = edit_status;
        move |book: LoadedBook| {
            edit_title.set(book.title.clone());
            edit_body.set(
                book.body
                    .clone()
                    .or_else(|| book.content.clone())
                    .unwrap_or_default(),
            );
            edit_tags.set("".into());
            edit_reason.set("Edit via Library page".into());
            edit_status.set(String::new());
            edit_book.set(Some(book));
        }
    };
    let save_book_edit = move |_| {
        let Some(book) = edit_book.get_untracked() else {
            return;
        };
        let title = edit_title.get_untracked().trim().to_owned();
        if title.is_empty() {
            edit_status.set("Title is required.".into());
            return;
        }
        let body = edit_body.get_untracked();
        let tags = edit_tags
            .get_untracked()
            .split(',')
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>();
        let reason = edit_reason.get_untracked().trim().to_owned();
        if reason.is_empty() {
            edit_status.set("Reason is required.".into());
            return;
        }
        edit_saving.set(true);
        edit_status.set("Saving...".into());
        let book_id = book.book_id.clone();
        let revision = book.revision;
        spawn_local(async move {
            let body_json = serde_json::json!({"title": title, "body": body, "tags": tags, "metadata": {}, "expected_revision": revision, "reason": reason,});
            let request =
                Request::put(&format!("/api/v1/library/books/{book_id}")).json(&body_json);
            let outcome = match request {
                Ok(req) => match req.send().await {
                    Ok(resp) if resp.ok() => Ok("Book updated.".to_owned()),
                    Ok(resp) => Err(api_error(&resp).await),
                    Err(_) => Err("Library service did not answer.".to_owned()),
                },
                Err(_) => Err("Edit request could not be encoded.".to_owned()),
            };
            edit_saving.set(false);
            match outcome {
                Ok(msg) => {
                    edit_status.set(msg);
                    edit_book.set(None);
                    load_library_list(all_books, status, None);
                    loaded.set(None);
                }
                Err(err) => edit_status.set(format!("Save failed: {err}")),
            }
        });
    };
    let close_book_edit = move |_| {
        edit_book.set(None);
        edit_status.set(String::new());
    };
    let open_mcp_edit = {
        let mcp_edit_id = mcp_edit_id;
        let mcp_edit_name = mcp_edit_name;
        let mcp_edit_config = mcp_edit_config;
        let mcp_edit_enabled = mcp_edit_enabled;
        let mcp_edit_status = mcp_edit_status;
        move |book: LoadedBook| {
            if let Some(server_id) = book.mcp_server_id.clone() {
                mcp_edit_id.set(Some(server_id));
                mcp_edit_name.set(book.name.clone().unwrap_or_default());
                mcp_edit_enabled.set(true);
                mcp_edit_config.set(String::new());
                mcp_edit_status.set(String::new());
            }
        }
    };
    let save_mcp_edit = move |_| {
        let Some(server_id) = mcp_edit_id.get_untracked() else {
            return;
        };
        let name = mcp_edit_name.get_untracked().trim().to_owned();
        if name.is_empty() {
            mcp_edit_status.set("Name is required.".into());
            return;
        }
        let config_text = mcp_edit_config.get_untracked().trim().to_owned();
        let config_value = if config_text.is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            match serde_json::from_str(&config_text) {
                Ok(v) => v,
                Err(e) => {
                    mcp_edit_status.set(format!("Invalid JSON: {e}"));
                    return;
                }
            }
        };
        let enabled = mcp_edit_enabled.get_untracked();
        mcp_edit_saving.set(true);
        mcp_edit_status.set("Saving MCP server...".into());
        spawn_local(async move {
            let body = serde_json::json!({"name": name, "configuration": config_value, "enabled": enabled});
            let request = Request::patch(&format!("/api/v1/mcp/servers/{server_id}")).json(&body);
            let outcome = match request {
                Ok(req) => match req.send().await {
                    Ok(resp) if resp.ok() => Ok("MCP server updated.".to_owned()),
                    Ok(resp) => Err(api_error(&resp).await),
                    Err(_) => Err("MCP service did not answer.".to_owned()),
                },
                Err(_) => Err("Update request could not be encoded.".to_owned()),
            };
            mcp_edit_saving.set(false);
            match outcome {
                Ok(msg) => {
                    mcp_edit_status.set(msg);
                    mcp_edit_id.set(None);
                    load_library_list(all_books, status, None);
                    loaded.set(None);
                }
                Err(err) => mcp_edit_status.set(format!("Save failed: {err}")),
            }
        });
    };
    let delete_mcp = move |server_id: String| {
        let confirmed = web_sys::window().is_some_and(|w| {
            w.confirm_with_message(&format!("Delete MCP server {server_id}?"))
                .unwrap_or(false)
        });
        if !confirmed {
            return;
        }
        spawn_local(async move {
            let request = Request::delete(&format!("/api/v1/mcp/servers/{server_id}"))
                .send()
                .await;
            match request {
                Ok(resp) if resp.ok() => {
                    status.set("MCP server deleted.".into());
                    load_library_list(all_books, status, None);
                    loaded.set(None);
                }
                Ok(resp) => status.set(format!("Delete failed: {}", api_error(&resp).await)),
                Err(_) => status.set("MCP service did not answer.".into()),
            }
        });
    };
    let close_mcp_edit = move |_| {
        mcp_edit_id.set(None);
        mcp_edit_status.set(String::new());
    };
    let delete_target_lib = RwSignal::new(None::<DeleteTarget>);
    let do_delete_book = {
        let all_books = all_books;
        let status = status;
        let loaded = loaded;
        std::sync::Arc::new(move |id: String| {
            spawn_local(async move {
                let resp = Request::delete(&format!("/api/v1/library/books/{id}"))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status() == 204 => {
                        status.set("Book deleted.".into());
                        load_library_list(all_books, status, None);
                        loaded.set(None);
                    }
                    Ok(r) if r.status() == 409 => {
                        let msg = api_error(&r).await;
                        status.set(format!("Cannot delete book: {msg}"));
                    }
                    Ok(r) if r.status() == 403 => {
                        status.set("Owner/admin role required to delete books.".into());
                    }
                    Ok(r) => {
                        let msg = api_error(&r).await;
                        if msg.to_lowercase().contains("forbidden")
                            || msg.to_lowercase().contains("admin")
                            || msg.to_lowercase().contains("owner")
                        {
                            status.set("Owner/admin role required to delete books.".into());
                        } else {
                            status.set(format!("Delete failed: {msg}"));
                        }
                    }
                    Err(_) => status.set("Library service did not answer.".into()),
                }
            });
        })
    };
    let do_delete_skill = {
        let all_books = all_books;
        let status = status;
        let loaded = loaded;
        std::sync::Arc::new(move |id: String| {
            spawn_local(async move {
                let resp = Request::delete(&format!("/api/v1/skills/{id}"))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status() == 204 => {
                        status.set("Skill deleted.".into());
                        load_library_list(all_books, status, None);
                        loaded.set(None);
                    }
                    Ok(r) if r.status() == 409 => {
                        let msg = api_error(&r).await;
                        status.set(format!("Cannot delete skill: {msg}"));
                    }
                    Ok(r) if r.status() == 403 => {
                        status.set("Owner/admin role required to delete skills.".into());
                    }
                    Ok(r) => {
                        let msg = api_error(&r).await;
                        if msg.to_lowercase().contains("forbidden")
                            || msg.to_lowercase().contains("admin")
                        {
                            status.set("Owner/admin role required to delete skills.".into());
                        } else {
                            status.set(format!("Delete failed: {msg}"));
                        }
                    }
                    Err(_) => status.set("Skills service did not answer.".into()),
                }
            });
        })
    };

    // ---- plugin install stepper ----
    let open_stepper = {
        move |source_uri: Option<String>, version: Option<String>| {
            stepper_source.set(
                source_uri
                    .unwrap_or_else(|| "https://github.com/".to_owned())
                    .trim()
                    .to_owned(),
            );
            stepper_version.set(version.unwrap_or_default());
            stepper_workspace.set(String::new());
            stepper.set(Some(InstallStepperState {
                step: StepperStep::Source,
                source_type: "github_release".into(),
                source_uri: String::new(),
                version: None,
                workspace_id: None,
                preview: None,
                install: None,
                confirmed: false,
                phase: String::new(),
                error: None,
            }));
        }
    };
    let close_stepper = move |_| stepper.set(None);
    let stepper_preview = {
        move |_| {
            let source_uri = stepper_source.get_untracked().trim().to_owned();
            if source_uri.is_empty() {
                stepper.update(|maybe_state| {
                    if let Some(state) = maybe_state.as_mut() {
                        state.error = Some(
                            "Enter a GitHub URL (https://github.com/owner/repo) or owner/repo."
                                .into(),
                        );
                    }
                });
                return;
            }
            let version = {
                let value = stepper_version.get_untracked().trim().to_owned();
                (!value.is_empty()).then_some(value)
            };
            let workspace_id = {
                let value = stepper_workspace.get_untracked();
                (!value.is_empty()).then_some(value)
            };
            stepper.update(|maybe_state| {
                if let Some(state) = maybe_state.as_mut() {
                    state.step = StepperStep::Previewing;
                    state.source_type = "github_release".into();
                    state.source_uri = source_uri.clone();
                    state.version = version.clone();
                    state.workspace_id = workspace_id.clone();
                    state.error = None;
                    state.preview = None;
                }
            });
            let stepper = stepper;
            spawn_local(async move {
                let request =
                    Request::post("/api/v1/plugins/preview").json(&PluginPreviewRequest {
                        source_type: "github_release",
                        source_uri: &source_uri,
                        version: version.as_deref(),
                    });
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<PluginPreview>().await {
                                Ok(preview) => {
                                    stepper.update(|maybe_state| {
                                        if let Some(state) = maybe_state.as_mut() {
                                            state.preview = Some(preview);
                                            state.step = StepperStep::Preview;
                                        }
                                    });
                                    Ok(())
                                }
                                Err(_) => Err("The preview response was not valid.".to_owned()),
                            }
                        }
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Plugin service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The preview request could not be encoded.".to_owned()),
                };
                if let Err(error) = outcome {
                    stepper.update(|maybe_state| {
                        if let Some(state) = maybe_state.as_mut() {
                            state.error = Some(error);
                            state.step = StepperStep::Error;
                        }
                    });
                }
            });
        }
    };
    let stepper_to_approve = {
        move |_| {
            stepper.update(|maybe_state| {
                if let Some(state) = maybe_state.as_mut() {
                    state.step = StepperStep::Approve;
                    state.confirmed = false;
                }
            });
        }
    };
    let stepper_back = {
        move |_| {
            stepper.update(|maybe_state| {
                if let Some(state) = maybe_state.as_mut() {
                    state.step = if state.install.is_some() {
                        StepperStep::Done
                    } else if state.preview.is_some() {
                        StepperStep::Preview
                    } else {
                        StepperStep::Source
                    };
                    state.error = None;
                }
            });
        }
    };
    let stepper_confirm = {
        move |checked: bool| {
            stepper.update(|maybe_state| {
                if let Some(state) = maybe_state.as_mut() {
                    state.confirmed = checked;
                }
            });
        }
    };
    let stepper_install = {
        move |_| {
            let Some(current) = stepper.get_untracked() else {
                return;
            };
            if !current.confirmed {
                stepper.update(|maybe_state| {
                    if let Some(state) = maybe_state.as_mut() {
                        state.error = Some(
                            "Confirm that you reviewed the manifest before installing.".into(),
                        );
                    }
                });
                return;
            }
            let Some(preview) = current.preview.clone() else {
                return;
            };
            let source_uri = current.source_uri.clone();
            let version = current.version.clone();
            let workspace_id = current.workspace_id.clone();
            stepper.update(|maybe_state| {
                if let Some(state) = maybe_state.as_mut() {
                    state.step = StepperStep::Installing;
                    state.phase = "Staging artifact...".into();
                    state.error = None;
                }
            });
            let stepper = stepper;
            spawn_local(async move {
                // Staged progress labels while the synchronous install runs.
                let labels = [
                    "Staging artifact...",
                    "Running sandbox self-test...",
                    "Installing components...",
                    "Finalizing...",
                ];
                for label in labels {
                    stepper.update(|maybe_state| {
                        if let Some(state) = maybe_state.as_mut()
                            && state.step == StepperStep::Installing
                        {
                            state.phase = label.into();
                        }
                    });
                    wait_for_poll(300).await;
                }
                let request =
                    Request::post("/api/v1/plugins/install").json(&PluginInstallRequest {
                        source_type: "github_release",
                        source_uri: &source_uri,
                        version: version.as_deref(),
                        expected_digest: &preview.source.digest,
                        approve: true,
                        workspace_id: workspace_id.as_deref(),
                    });
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<InstallResponse>().await {
                                Ok(installed) => {
                                    stepper.update(|maybe_state| {
                                        if let Some(state) = maybe_state.as_mut() {
                                            state.install = Some(installed);
                                            state.step = StepperStep::Done;
                                        }
                                    });
                                    Ok(())
                                }
                                Err(_) => Err("The install response was not valid.".to_owned()),
                            }
                        }
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Plugin service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The install request could not be encoded.".to_owned()),
                };
                if let Err(error) = outcome {
                    stepper.update(|maybe_state| {
                        if let Some(state) = maybe_state.as_mut() {
                            state.error = Some(error);
                            state.step = StepperStep::Error;
                        }
                    });
                }
                load_library_list(all_books, status, None);
                plugin_detail_tick.update(|tick| *tick = tick.wrapping_add(1));
            });
        }
    };
    let stepper_enable = {
        move |_| {
            let Some(installed) = stepper
                .get_untracked()
                .and_then(|state| state.install.clone())
            else {
                return;
            };
            stepper.update(|maybe_state| {
                if let Some(state) = maybe_state.as_mut() {
                    state.phase = "Enabling...".into();
                }
            });
            let stepper = stepper;
            spawn_local(async move {
                let request = Request::patch(&format!("/api/v1/plugins/{}", installed.plugin_id))
                    .json(&PatchPluginRequest {
                        state: Some("enabled"),
                        trust: None,
                    });
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            stepper.update(|maybe_state| {
                                if let Some(state) = maybe_state.as_mut() {
                                    state.step = StepperStep::Done;
                                    state.phase = "Enabled.".into();
                                }
                            });
                            Ok(())
                        }
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Plugin service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The enable request could not be encoded.".to_owned()),
                };
                if let Err(error) = outcome {
                    stepper.update(|maybe_state| {
                        if let Some(state) = maybe_state.as_mut() {
                            state.error = Some(error);
                            state.step = StepperStep::Error;
                        }
                    });
                }
                load_library_list(all_books, status, None);
                plugin_detail_tick.update(|tick| *tick = tick.wrapping_add(1));
            });
        }
    };

    // ---- marketplace ----
    let open_marketplace = {
        move |_| {
            marketplace_open.set(true);
            marketplace_results.set(Vec::new());
            marketplace_status.set("Search the marketplace to see installable plugins.".into());
        }
    };
    let close_marketplace = move |_| marketplace_open.set(false);
    let marketplace_search = {
        move |event: leptos::ev::SubmitEvent| {
            event.prevent_default();
            let value = marketplace_query.get_untracked().trim().to_owned();
            if value.is_empty() {
                marketplace_status.set("Enter a marketplace query.".into());
                return;
            }
            marketplace_loading.set(true);
            marketplace_status.set("Searching the marketplace...".into());
            let marketplace_results = marketplace_results;
            let marketplace_status = marketplace_status;
            let marketplace_loading = marketplace_loading;
            spawn_local(async move {
                let request = Request::post("/api/v1/plugins/search")
                    .json(&PluginSearchRequest { query: &value });
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<Vec<MarketplaceResult>>().await {
                                Ok(found) => {
                                    marketplace_results.set(found);
                                    Ok("Marketplace results loaded.".to_owned())
                                }
                                Err(_) => Err("The marketplace response was not valid.".to_owned()),
                            }
                        }
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Marketplace service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The search request could not be encoded.".to_owned()),
                };
                marketplace_loading.set(false);
                match outcome {
                    Ok(message) => marketplace_status.set(message),
                    Err(error) => marketplace_status.set(format!("Search failed: {error}")),
                }
            });
        }
    };
    let marketplace_pick = {
        move |result: MarketplaceResult| {
            // The marketplace reports github_release sources
            // (`github_release:owner/repo`); the server preview/install only
            // accept `github_release`, so pass its URL + no pinned version
            // (resolves the latest release).
            open_stepper(Some(result.source_uri), None);
            marketplace_open.set(false);
        }
    };

    // ---- plugin detail ----
    let open_plugin = { move |plugin_id: String| plugin_detail_id.set(Some(plugin_id)) };

    let _active_conv = active_conversation();

    view! {
        {move || plugin_detail_id.get().map(|plugin_id| {
            let plugin_id = plugin_id.clone();
            let on_back: std::sync::Arc<dyn Fn() + Send + Sync + 'static> = {
                std::sync::Arc::new(move || plugin_detail_id.set(None))
            };
            let on_upgrade_done: std::sync::Arc<dyn Fn() + Send + Sync + 'static> = {
                std::sync::Arc::new(move || plugin_detail_tick.update(|tick| *tick = tick.wrapping_add(1)))
            };
            let on_changed: std::sync::Arc<dyn Fn() + Send + Sync + 'static> =
                std::sync::Arc::new(refresh);
            view! {
                <PluginDetailView plugin_id=plugin_id on_back on_upgrade_done on_changed />
            }.into_any()
        })}
        {move || if plugin_detail_id.get().is_none() && marketplace_open.get() {
            view! {
                <div class="page-heading">
                    <div><p class="utility">"CONNECT / PLUGIN MARKETPLACE"</p><h1>"Marketplace"</h1></div>
                    <button class="text-button" on:click=close_marketplace>"← Back to Library"</button>
                </div>
                <form class="filter-row" on:submit=marketplace_search>
                    <input placeholder="Search plugins (e.g. mcp-server, claude, tools)" prop:value=move || marketplace_query.get()
                        on:input=move |event| marketplace_query.set(event_target_value(&event)) />
                    <button type="submit" disabled=move || marketplace_loading.get()>
                        {move || if marketplace_loading.get() { "Searching..." } else { "Search" }}
                    </button>
                </form>
                <p class="form-note">{move || marketplace_status.get()}</p>
                <div class="book-grid">
                    {move || marketplace_results.get().into_iter().map(|result| {
                        let result_clone = result.clone();
                        let pick = marketplace_pick;
                        view! {
                            <article class="book-card">
                                <div class="book-card-head">
                                    <strong>{result.name}</strong>
                                    <span class="kind-badge plugin">"PLUGIN"</span>
                                </div>
                                <p>{result.description}</p>
                                <div class="book-card-meta">
                                    <span class="spine-cell">{result.publisher}</span>
                                    <span class="cap-chip">{result.version}</span>
                                    <span class="cap-chip">{format!("{} downloads", result.popularity)}</span>
                                    <span class="trust-badge untrusted">"UNTRUSTED"</span>
                                </div>
                                <button class="text-button" on:click=move |_| pick(result_clone.clone())>"Preview & install →"</button>
                            </article>
                        }
                    }).collect_view()}
                </div>
            }.into_any()
        } else if plugin_detail_id.get().is_none() {
            view! {
                <div class="page-heading">
                    <div><p class="utility">"GLOBAL CONTEXT / LIBRARY"</p><h1>"Library"</h1></div>
                    <span class="utility">{move || format!("{} RESULTS", visible_book_count(all_books, search_hits, filter))}</span>
                </div>
                <div class="library-toolbar">
                    <div class="kind-tabs" role="tablist" aria-label="Library kind filter">
                        {[LibraryFilter::All, LibraryFilter::Source, LibraryFilter::Skill, LibraryFilter::Plugin, LibraryFilter::Mcp]
                            .into_iter().map(|candidate| {
                                view! {
                                    <button class:active=move || filter.get() == candidate
                                        role="tab" aria-selected=move || filter.get() == candidate
                                        on:click=move |_| set_filter(candidate)>
                                        {candidate.label()}
                                    </button>
                                }
                            }).collect_view()}
                    </div>
                    <div class="library-actions">
                        <button class="text-button" on:click=move |_| page.set(Page::Autobiography)>"Autobiography →"</button>
                        <button class="text-button" on:click=open_marketplace>"Browse marketplace"</button>
                        <div class="new-menu">
                            <button class="secondary" on:click=move |_| new_menu.set(!new_menu.get_untracked())>"New ▾"</button>
                            {move || new_menu.get().then(|| view! {
                                <div class="new-menu-list">
                                    <button type="button" on:click=move |_| { new_menu.set(false); new_form.set(Some("source")); }>"New Source Book"</button>
                                    <button type="button" on:click=move |_| { new_menu.set(false); new_form.set(Some("skill")); }>"New Skill"</button>
                                    <button type="button" on:click=move |_| { new_menu.set(false); new_form.set(Some("mcp")); }>"New MCP Server"</button>
                                    <button type="button" on:click=move |_| { new_menu.set(false); open_stepper(None, None); }>"Install Plugin"</button>
                                </div>
                            })}
                        </div>
                    </div>
                </div>
                <form class="filter-row" on:submit=run_search>
                    <input placeholder="Lexical + semantic search across all kinds" prop:value=move || query.get()
                        on:input=move |event| query.set(event_target_value(&event)) />
                    <button type="submit">"Search"</button>
                    <button type="button" on:click=move |_| refresh()>"Reset"</button>
                </form>
                <p class="form-note">{move || status.get()}</p>
                {move || new_form.get().map(|form| {
                    view! {
                        <section class="model-section">
                            <div class="section-heading">
                                <div><p class="utility">"LIBRARY / NEW"</p><h2>{
                                    match form { "source" => "New Source Book", "skill" => "New Skill", _ => "New MCP Server" }
                                }</h2></div>
                                <button class="text-button" on:click=move |_| new_form.set(None)>"Close"</button>
                            </div>
                            {if form == "source" {
                                view! {
                                    <form class="model-form" on:submit=create_book>
                                        <label>"Title"<input required maxlength="500" prop:value=move || create_title.get() on:input=move |event| create_title.set(event_target_value(&event)) /></label>
                                        <label>"Tags (comma separated)"<input maxlength="500" placeholder="tag1, tag2" prop:value=move || create_tags.get() on:input=move |event| create_tags.set(event_target_value(&event)) /></label>
                                        <label>"Scope"
                                            <select prop:value=move || create_scope.get()
                                                on:change=move |event| create_scope.set(event_target_value(&event))>
                                                <option value="PROFILE">"PROFILE"</option>
                                                <option value="USER">"USER"</option>
                                                <option value="PRIVATE">"PRIVATE"</option>
                                                <option value="WORKSPACE">"WORKSPACE"</option>
                                            </select>
                                        </label>
                                        <label class="fallback-field">"Body"
                                            <textarea rows="8" maxlength="100000" placeholder="Book contents" prop:value=move || create_body.get() on:input=move |event| create_body.set(event_target_value(&event))></textarea>
                                        </label>
                                        <div>
                                            <button class="primary" type="submit" disabled=move || creating.get()>
                                                {move || if creating.get() { "Creating..." } else { "Create source book" }}
                                            </button>
                                        </div>
                                        <p class="form-note">{move || create_status.get()}</p>
                                    </form>
                                }.into_any()
                            } else if form == "skill" {
                                view! {
                                    <form class="model-form" on:submit=create_skill>
                                        <label>"Name"<input required maxlength="200" prop:value=move || skill_name.get() on:input=move |event| skill_name.set(event_target_value(&event)) /></label>
                                        <label class="fallback-field">"Description"
                                            <input maxlength="10000" placeholder="What the skill does" prop:value=move || skill_description.get() on:input=move |event| skill_description.set(event_target_value(&event)) />
                                        </label>
                                        <label class="fallback-field">"Instructions (content)"
                                            <textarea rows="10" maxlength="100000" placeholder="Procedure content for the first revision" prop:value=move || skill_content.get() on:input=move |event| skill_content.set(event_target_value(&event))></textarea>
                                        </label>
                                        <div>
                                            <button class="primary" type="submit" disabled=move || skill_creating.get()>
                                                {move || if skill_creating.get() { "Creating..." } else { "Create skill" }}
                                            </button>
                                        </div>
                                        <p class="form-note">{move || skill_status.get()}</p>
                                    </form>
                                }.into_any()
                            } else {
                                view! {
                                    <form class="model-form" on:submit=create_mcp>
                                        <label>"Name"<input required maxlength="200" prop:value=move || mcp_name.get() on:input=move |event| mcp_name.set(event_target_value(&event)) /></label>
                                        <label>"Transport"
                                            <select prop:value=move || mcp_transport.get()
                                                on:change=move |event| mcp_transport.set(event_target_value(&event))>
                                                <option value="stdio">"stdio"</option>
                                                <option value="streamable_http">"streamable_http"</option>
                                            </select>
                                        </label>
                                        <label class="fallback-field">"Configuration (JSON)"
                                            <textarea rows="8" placeholder="{\"command\": \"npx\", \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\"]}" prop:value=move || mcp_configuration.get() on:input=move |event| mcp_configuration.set(event_target_value(&event))></textarea>
                                        </label>
                                        <div>
                                            <button class="primary" type="submit" disabled=move || mcp_creating.get()>
                                                {move || if mcp_creating.get() { "Creating..." } else { "Create MCP server" }}
                                            </button>
                                        </div>
                                        <p class="form-note">{move || mcp_status.get()}</p>
                                    </form>
                                }.into_any()
                            }}
                        </section>
                    }.into_any()
                })}
                <p class="form-note">{move || load_status.get()}</p>
                <div class="book-grid">
                    <p class="form-note">"GRID-DEBUG: "{move || format!("{}", {
                        let kind_filter = filter.get();
                        let list = match search_hits.get() {
                            Some(hits) => hits,
                            None => all_books.get().into_iter().filter(|book| kind_filter.matches(&book.kind)).collect::<Vec<_>>(),
                        };
                        list.len()
                    })}</p>
                    {move || match search_hits.get() {
                        Some(hits) => hits
                            .into_iter()
                            .map(|book| view! {
                                <article class="book-card">
                                    <div class="book-card-head">
                                        <span class="kind-badge plugin">{book.kind.clone().unwrap_or_else(|| "SOURCE".into())}</span>
                                        <strong>{book.title.clone()}</strong>
                                        <span class="trust-badge untrusted">{book.trust.clone()}</span>
                                    </div>
                                    <p class="book-snippet">{book.snippet.clone()}</p>
                                    <div class="book-card-meta">
                                        {book.tags.iter().take(4).map(|tag| view! { <span class="cap-chip">{tag.clone()}</span> }).collect_view()}
                                        {book.capabilities.iter().take(4).map(|cap| view! { <span class="cap-chip">{cap.clone()}</span> }).collect_view()}
                                    </div>
                                    <div class="book-card-actions">
                                        <button class="text-button" on:click={
                                            let book_id = book.id.clone();
                                            move |_| open_book(book_id.clone())
                                        }>"Open detail"</button>
                                        <button class="text-button" on:click={
                                            let book_id = book.id.clone();
                                            move |_| pin_book(book_id.clone())
                                        }>"Pin to conversation"</button>
                                        <button class="text-button" on:click={
                                            let book_id = book.id.clone();
                                            let open_book_edit = open_book_edit;
                                            move |_| {
                                                let open_book_edit = open_book_edit;
                                                let bid = book_id.clone();
                                                spawn_local(async move {
                                                    if let Ok(found) = load_library_book(&bid).await {
                                                        open_book_edit(found);
                                                    }
                                                });
                                            }
                                        }>"Edit"</button>
                                        <button class="icon-button danger" type="button" title="Delete book" aria-label=format!("Delete book {}", book.title.clone())
                                            on:click={
                                                let id = book.id.clone();
                                                let title = book.title.clone();
                                                let kind = book.kind.clone();
                                                move |_| {
                                                    if kind.as_deref() == Some("SKILL") {
                                                        delete_target_lib.set(Some(DeleteTarget::Skill { id: id.clone(), label: title.clone() }));
                                                    } else {
                                                        delete_target_lib.set(Some(DeleteTarget::Book { id: id.clone(), label: title.clone() }));
                                                    }
                                                }
                                            }>"🗑"</button>
                                    </div>
                                </article>
                            })
                            .collect_view()
                            .into_any(),
                        None => all_books
                            .get()
                            .into_iter()
                            .filter(|book| filter.get().matches(&book.kind))
                            .map(|book| view! {
                                <article class="book-card">
                                    <div class="book-card-head">
                                        <span class="kind-badge plugin">{book.kind.clone().unwrap_or_else(|| "SOURCE".into())}</span>
                                        <strong>{book.title.clone()}</strong>
                                        <span class="trust-badge untrusted">{book.trust.clone()}</span>
                                    </div>
                                    <p class="book-snippet">{book.snippet.clone()}</p>
                                    <div class="book-card-meta">
                                        {book.tags.iter().take(4).map(|tag| view! { <span class="cap-chip">{tag.clone()}</span> }).collect_view()}
                                        {book.capabilities.iter().take(4).map(|cap| view! { <span class="cap-chip">{cap.clone()}</span> }).collect_view()}
                                    </div>
                                    <div class="book-card-actions">
                                        <button class="text-button" on:click={
                                            let book_id = book.id.clone();
                                            move |_| open_book(book_id.clone())
                                        }>"Open detail"</button>
                                        <button class="text-button" on:click={
                                            let book_id = book.id.clone();
                                            move |_| pin_book(book_id.clone())
                                        }>"Pin to conversation"</button>
                                        <button class="text-button" on:click={
                                            let book_id = book.id.clone();
                                            let open_book_edit = open_book_edit;
                                            move |_| {
                                                let open_book_edit = open_book_edit;
                                                let bid = book_id.clone();
                                                spawn_local(async move {
                                                    if let Ok(found) = load_library_book(&bid).await {
                                                        open_book_edit(found);
                                                    }
                                                });
                                            }
                                        }>"Edit"</button>
                                        <button class="icon-button danger" type="button" title="Delete book" aria-label=format!("Delete book {}", book.title.clone())
                                            on:click={
                                                let id = book.id.clone();
                                                let title = book.title.clone();
                                                let kind = book.kind.clone();
                                                move |_| {
                                                    if kind.as_deref() == Some("SKILL") {
                                                        delete_target_lib.set(Some(DeleteTarget::Skill { id: id.clone(), label: title.clone() }));
                                                    } else {
                                                        delete_target_lib.set(Some(DeleteTarget::Book { id: id.clone(), label: title.clone() }));
                                                    }
                                                }
                                            }>"🗑"</button>
                                    </div>
                                </article>
                            })
                            .collect_view()
                            .into_any(),
                    }}
                </div>
                {move || loaded.get().map(|book| {
                    let close = move |_| loaded.set(None);
                    view! {
                        <section class="model-section book-detail">
                            <div class="section-heading">
                                <div><p class="utility">"LIBRARY / DETAIL"</p><h2>{book.title.clone()}</h2></div>
                                <div style="display:flex;gap:8px;align-items:center;">
                                    <span class="spine-cell">{book.kind.clone().unwrap_or_else(|| "SOURCE".into())}</span>
                                    <button class="secondary" type="button" on:click={
                                        let b = book.clone();
                                        let open_book_edit = open_book_edit;
                                        move |_| open_book_edit(b.clone())
                                    }>"Edit book"</button>
                                    {if book.kind.as_deref() == Some("MCP") {
                                        let b = book.clone();
                                        let open_mcp_edit = open_mcp_edit;
                                        let sid = book.mcp_server_id.clone().unwrap_or_default();
                                        let delete_mcp = delete_mcp;
                                        view! {
                                            <button class="secondary" type="button" on:click=move |_| open_mcp_edit(b.clone())>"Edit MCP"</button>
                                            <button class="secondary danger" type="button" on:click=move |_| delete_mcp(sid.clone())>"Delete MCP"</button>
                                        }.into_any()
                                    } else if book.kind.as_deref() == Some("SKILL") {
                                        let sid = book.skill_id.clone().unwrap_or_else(|| book.book_id.clone());
                                        let stitle = book.title.clone();
                                        view! {
                                            <button class="secondary danger" type="button" title="Delete skill" on:click=move |_| delete_target_lib.set(Some(DeleteTarget::Skill { id: sid.clone(), label: stitle.clone() }))>"Delete skill"</button>
                                        }.into_any()
                                    } else {
                                        let bid = book.book_id.clone();
                                        let btitle = book.title.clone();
                                        view! {
                                            <button class="secondary danger" type="button" title="Delete book" on:click=move |_| delete_target_lib.set(Some(DeleteTarget::Book { id: bid.clone(), label: btitle.clone() }))>"Delete book"</button>
                                        }.into_any()
                                    }}
                                    <button class="text-button" on:click=close>"Close"</button>
                                </div>
                            </div>
                            <LoadedBookDetail book=book.clone() on_open_plugin=open_plugin />
                        </section>
                    }.into_any()
                })}
            }.into_any()
        } else {
            view! { <span class="utility"></span> }.into_any()
        }}
        {move || edit_book.get().map(|_book| {
            view! {
                <div class="modal-backdrop" role="presentation">
                    <section class="modal" role="dialog" aria-modal="true" aria-label="Edit book">
                        <div class="modal-head"><p class="utility">"LIBRARY / EDIT BOOK"</p><button class="text-button" on:click=close_book_edit>"Close"</button></div>
                        <label>"Title"<input required maxlength="500" prop:value=move || edit_title.get() on:input=move |e| edit_title.set(event_target_value(&e)) /></label>
                        <label>"Body / Content"<textarea rows="8" prop:value=move || edit_body.get() on:input=move |e| edit_body.set(event_target_value(&e))></textarea></label>
                        <label>"Tags (comma separated)"<input maxlength="500" placeholder="tag1, tag2" prop:value=move || edit_tags.get() on:input=move |e| edit_tags.set(event_target_value(&e)) /></label>
                        <label>"Reason"<input required maxlength="500" prop:value=move || edit_reason.get() on:input=move |e| edit_reason.set(event_target_value(&e)) /></label>
                        <p class="form-note">{move || edit_status.get()}</p>
                        <div class="stepper-actions">
                            <button class="secondary" type="button" on:click=close_book_edit>"Cancel"</button>
                            <button class="primary" type="button" disabled=move || edit_saving.get() on:click=save_book_edit>{move || if edit_saving.get() { "Saving..." } else { "Save" }}</button>
                        </div>
                    </section>
                </div>
            }.into_any()
        })}
        {move || mcp_edit_id.get().as_ref().map(|sid| {
            let sid = sid.clone();
            view! {
                <div class="modal-backdrop" role="presentation">
                    <section class="modal" role="dialog" aria-modal="true" aria-label="Edit MCP server">
                        <div class="modal-head"><p class="utility">"LIBRARY / EDIT MCP"</p><button class="text-button" on:click=close_mcp_edit>"Close"</button></div>
                        <p class="form-note"><span>"Server: "</span><span>{sid.clone()}</span></p>
                        <label>"Name"<input required maxlength="200" prop:value=move || mcp_edit_name.get() on:input=move |e| mcp_edit_name.set(event_target_value(&e)) /></label>
                        <label>"Configuration (JSON)"<textarea rows="8" placeholder="{}" prop:value=move || mcp_edit_config.get() on:input=move |e| mcp_edit_config.set(event_target_value(&e))></textarea></label>
                        <label class="confirm-row"><input type="checkbox" prop:checked=move || mcp_edit_enabled.get() on:change=move |e| mcp_edit_enabled.set(event_target_checked(&e)) /><span>"Enabled"</span></label>
                        <p class="form-note">{move || mcp_edit_status.get()}</p>
                        <div class="stepper-actions">
                            <button class="secondary" type="button" on:click=close_mcp_edit>"Cancel"</button>
                            <button class="primary" type="button" disabled=move || mcp_edit_saving.get() on:click=save_mcp_edit>{move || if mcp_edit_saving.get() { "Saving..." } else { "Save MCP" }}</button>
                        </div>
                    </section>
                </div>
            }.into_any()
        })}
        {move || {
            let db = std::sync::Arc::clone(&do_delete_book);
            let ds = std::sync::Arc::clone(&do_delete_skill);
            delete_target_lib.get().clone().map(|target| {
                let (title, body, is_skill) = match target.clone() {
                    DeleteTarget::Book { id, label } => (format!("Delete \"{label}\"?"), format!("This will permanently delete the book. Managed books (SKILL/PLUGIN/MCP) may be refused (409). Id: {}", &id[..8.min(id.len())]), false),
                    DeleteTarget::Skill { id, label } => (format!("Delete skill \"{label}\"?"), format!("This will soft-delete the skill and its book. Id: {}", &id[..8.min(id.len())]), true),
                    _ => (String::new(), String::new(), false),
                };
                let confirm_target = target.clone();
                let db_inner = std::sync::Arc::clone(&db);
                let ds_inner = std::sync::Arc::clone(&ds);
                view! {
                    <div class="modal-backdrop" role="dialog" aria-modal="true">
                        <div class="modal delete-confirm">
                            <div class="modal-head"><h3>{title.clone()}</h3><button class="text-button" on:click=move |_| delete_target_lib.set(None)>"✕"</button></div>
                            <p class="form-note">{body.clone()}</p>
                            <p class="error-note" style="font-size:13px">{if is_skill { "Skill revisions will be archived; the book will be removed." } else { "This cannot be undone." }}</p>
                            <div style="display:flex;gap:10px;justify-content:flex-end;margin-top:12px">
                                <button class="secondary" on:click=move |_| delete_target_lib.set(None)>"Cancel"</button>
                                <button class="primary danger" on:click=move |_| {
                                    match confirm_target.clone() {
                                        DeleteTarget::Book { id, .. } => db_inner(id),
                                        DeleteTarget::Skill { id, .. } => ds_inner(id),
                                        _ => {}
                                    }
                                    delete_target_lib.set(None);
                                }>"Delete"</button>
                            </div>
                        </div>
                    </div>
                }
            })
        }}
        {move || stepper.get().map(|state| {
            let state = state.clone();
            let close = close_stepper;
            let preview = stepper_preview;
            let to_approve = stepper_to_approve;
            let back = stepper_back;
            let confirm = stepper_confirm;
            let install = stepper_install;
            let enable = stepper_enable;
            view! {
                <div class="modal-backdrop" role="presentation">
                    <section class="modal" role="dialog" aria-modal="true" aria-label="Install plugin">
                        <div class="modal-head">
                            <p class="utility">"PLUGIN INSTALL / STEPPER"</p>
                            <button class="text-button" on:click=close>"Close"</button>
                        </div>
                        {match state.step {
                            StepperStep::Source => view! {
                                <>
                                    <div class="stepper-step"><span class="utility">"STEP 1 / 4 · SOURCE"</span></div>
                                    <label>"GitHub source"
                                        <input required maxlength="2048" placeholder="https://github.com/owner/repo or owner/repo"
                                            prop:value=move || stepper_source.get()
                                            on:input=move |event| stepper_source.set(event_target_value(&event)) />
                                    </label>
                                    <label>"Version (optional, defaults to the latest release)"
                                        <input maxlength="64" placeholder="1.2.3" prop:value=move || stepper_version.get()
                                            on:input=move |event| stepper_version.set(event_target_value(&event)) />
                                    </label>
                                    <label>"Install into workspace (optional; profile scope requires an OWNER/ADMIN role)"
                                        <select prop:value=move || stepper_workspace.get()
                                            on:change=move |event| stepper_workspace.set(event_target_value(&event))>
                                            <option value="">"Profile scope"</option>
                                            {workspaces.get().into_iter().map(|ws| {
                                                let id = ws.id.clone();
                                                let title = ws.title.clone();
                                                view! { <option value=id>{title}</option> }
                                            }).collect_view()}
                                        </select>
                                    </label>
                                    {state.error.clone().map(|error| view! { <p class="form-note error-note">{error}</p> })}
                                    <div class="stepper-actions">
                                        <button class="secondary" type="button" on:click=close>"Cancel"</button>
                                        <button class="primary" type="button" on:click=preview>"Preview manifest"</button>
                                    </div>
                                </>
                            }.into_any(),
                            StepperStep::Previewing => view! {
                                <div class="stepper-loading">
                                    <p class="utility">"STEP 2 / 4 · RESOLVING MANIFEST"</p>
                                    <p>"Fetching and validating the plugin manifest..."</p>
                                </div>
                            }.into_any(),
                            StepperStep::Preview => view! {
                                <>
                                    <div class="stepper-step"><span class="utility">"STEP 2 / 4 · MANIFEST PREVIEW"</span></div>
                                    {state.preview.clone().map(|preview| {
                                        view! {
                                            <ManifestPreview preview=preview.clone() />
                                        }
                                    })}
                                    {state.error.clone().map(|error| view! { <p class="form-note error-note">{error}</p> })}
                                    <div class="stepper-actions">
                                        <button class="secondary" type="button" on:click=back>"Back"</button>
                                        <button class="primary" type="button" on:click=to_approve>"Review approval →"</button>
                                    </div>
                                </>
                            }.into_any(),
                            StepperStep::Approve => view! {
                                <>
                                    <div class="stepper-step"><span class="utility">"STEP 3 / 4 · APPROVAL"</span></div>
                                    <p class="stepper-note">"Installing runs the plugin's self-test inside the sandbox (if declared) and grants the permission scope listed above. Permission values are references only — never secret material."</p>
                                    {state.preview.clone().map(|preview| {
                                        if preview.trust == "UNTRUSTED" {
                                            view! { <p class="form-note error-note">"Unknown publisher — verify the source commit and digest before install."</p> }.into_any()
                                        } else {
                                            view! { <span></span> }.into_any()
                                        }
                                    })}
                                    <label class="confirm-row">
                                        <input type="checkbox" prop:checked=move || state.confirmed
                                            on:change=move |event| confirm(event_target_checked(&event)) />
                                        <span>"I reviewed the manifest and approve installing this plugin."</span>
                                    </label>
                                    {state.error.clone().map(|error| view! { <p class="form-note error-note">{error}</p> })}
                                    <div class="stepper-actions">
                                        <button class="secondary" type="button" on:click=back>"Back"</button>
                                        <button class="primary" type="button" disabled=move || !state.confirmed on:click=install>"Approve & Install"</button>
                                    </div>
                                </>
                            }.into_any(),
                            StepperStep::Installing => view! {
                                <div class="stepper-loading">
                                    <p class="utility">"STEP 4 / 4 · INSTALLING"</p>
                                    <p>{state.phase.clone()}</p>
                                </div>
                            }.into_any(),
                            StepperStep::Done => view! {
                                <>
                                    <div class="stepper-step"><span class="utility">"INSTALLED · DORMANT"</span></div>
                                    {state.install.clone().map(|installed| {
                                        let installed = installed.clone();
                                        view! {
                                            <p class="stepper-note">"The plugin is installed and dormant: it cannot execute until enabled."</p>
                                            <div class="index-table">
                                                <div class="index-row"><span class="spine-cell">"PLUGIN"</span><span>{installed.plugin_id.clone()}</span></div>
                                                <div class="index-row"><span class="spine-cell">"BOOK"</span><span>{installed.book_id.clone()}</span></div>
                                                <div class="index-row"><span class="spine-cell">"STATE"</span><span>{installed.state.to_uppercase()}</span></div>
                                                <div class="index-row"><span class="spine-cell">"TRUST"</span><span>{installed.trust}</span></div>
                                            </div>
                                            <div class="stepper-actions">
                                                <button class="secondary" type="button" on:click=close>"Close"</button>
                                                <button class="primary" type="button" on:click=enable>
                                                    {move || if state.phase == "Enabling..." { "Enabling..." } else { "Enable" }}
                                                </button>
                                            </div>
                                        }
                                    })}
                                </>
                            }.into_any(),
                            StepperStep::Error => view! {
                                <>
                                    <div class="stepper-step"><span class="utility">"INSTALL FAILED"</span></div>
                                    {state.error.clone().map(|error| view! {
                                        <div class="inline-error" role="alert">
                                            <p>{error}</p>
                                            <p class="form-note">"No changes were made. Fix the reported issue and preview again."</p>
                                        </div>
                                    })}
                                    <div class="stepper-actions">
                                        <button class="secondary" type="button" on:click=back>"Back"</button>
                                        <button class="secondary" type="button" on:click=close>"Close"</button>
                                    </div>
                                </>
                            }.into_any(),
                        }}
                    </section>
                </div>
            }.into_any()
        })}
    }
}

#[component]
fn ManifestPreview(preview: PluginPreview) -> impl IntoView {
    view! {
        <div class="manifest-preview">
            <div class="manifest-head">
                <div>
                    <h3>{format!("{} · {}", preview.name.clone(), preview.version.clone())}</h3>
                    <p>{preview.description.clone()}</p>
                </div>
                <span class="trust-badge untrusted">"UNTRUSTED"</span>
            </div>
            <div class="index-table">
                <div class="index-row"><span class="spine-cell">"PUBLISHER"</span><span>{preview.publisher.clone()}</span></div>
                <div class="index-row"><span class="spine-cell">"SOURCE"</span><span>{preview.source.source_uri.clone()}</span></div>
                <div class="index-row"><span class="spine-cell">"COMMIT"</span><span class="mono-break">{preview.source.commit_sha.clone().unwrap_or_else(|| "—".into())}</span></div>
                <div class="index-row"><span class="spine-cell">"DIGEST"</span><span class="mono-break">{preview.source.digest.clone()}</span></div>
                <div class="index-row"><span class="spine-cell">"NETWORK"</span><span>{preview.network_policy.clone()}</span></div>
                {preview.self_test.clone().map(|test| view! {
                    <div class="index-row"><span class="spine-cell">"SELF-TEST"</span><span>{format!("{} ({}s timeout)", test.command.join(" "), test.timeout)}</span></div>
                })}
            </div>
            <p class="utility">"COMPONENTS"</p>
            <div class="index-table">
                {preview.components.iter().map(|component| {
                    view! { <div class="index-row component-row">
                        <span class="spine-cell">{component.component_type.to_uppercase()}</span>
                        <strong>{component.name.clone()}</strong>
                        <span>{component.component_ref.clone()}</span>
                        <span>{component.description.clone().unwrap_or_default()}</span>
                    </div> }
                }).collect_view()}
            </div>
            <p class="utility">"PERMISSIONS (REFERENCES ONLY)"</p>
            <div class="index-table">
                {preview.permissions.iter().map(|permission| {
                    view! { <div class="index-row component-row">
                        <span class="spine-cell">{permission.domain.to_uppercase()}</span>
                        <span>{permission.scope_value.clone()}</span>
                        <span></span>
                        <span></span>
                    </div> }
                }).collect_view()}
            </div>
        </div>
    }
}

#[component]
fn LoadedBookDetail(
    book: LoadedBook,
    on_open_plugin: impl Fn(String) + Clone + 'static,
) -> impl IntoView {
    let kind = book.kind.clone().unwrap_or_else(|| "SOURCE".into());
    let open_plugin = on_open_plugin.clone();
    view! {
        {match kind.as_str() {
            "SKILL" => view! {
                <div class="book-detail-body">
                    <div class="index-table">
                        <div class="index-row"><span class="spine-cell">"NAME"</span><span>{book.name.clone().unwrap_or_default()}</span></div>
                        <div class="index-row"><span class="spine-cell">"REVISION"</span><span>{format!("rev {}", book.revision)}</span></div>
                        <div class="index-row"><span class="spine-cell">"PROMOTED"</span><span>{if book.promoted.unwrap_or(false) { "YES" } else { "NO" }}</span></div>
                    </div>
                    <p class="utility">"INSTRUCTIONS"</p>
                    <pre>{book.content.clone().unwrap_or_default()}</pre>
                </div>
            }.into_any(),
            "MCP" => view! {
                <div class="book-detail-body">
                    <div class="index-table">
                        <div class="index-row"><span class="spine-cell">"NAME"</span><span>{book.name.clone().unwrap_or_default()}</span></div>
                        <div class="index-row"><span class="spine-cell">"TRANSPORT"</span><span>{book.transport.clone().unwrap_or_default().to_uppercase()}</span></div>
                    </div>
                    <p class="utility">"DISCOVERED TOOLS"</p>
                    <div class="index-table">
                        {book.tools.iter().map(|tool| {
                            view! { <div class="index-row component-row">
                                <span class="spine-cell">"TOOL"</span>
                                <strong>{tool.name.clone()}</strong>
                                <span>{tool.description.clone()}</span>
                                <span></span>
                            </div> }
                        }).collect_view()}
                    </div>
                </div>
            }.into_any(),
            "PLUGIN" => view! {
                <div class="book-detail-body">
                    <div class="index-table">
                        <div class="index-row"><span class="spine-cell">"NAME"</span><span>{book.name.clone().unwrap_or_default()}</span></div>
                        <div class="index-row"><span class="spine-cell">"VERSION"</span><span>{book.version.clone().unwrap_or_default()}</span></div>
                    </div>
                    <p class="utility">"COMPONENTS"</p>
                    <div class="index-table">
                        {book.components.iter().map(|component| {
                            view! { <div class="index-row component-row">
                                <span class="spine-cell">{component.component_type.to_uppercase()}</span>
                                <strong>{component.name.clone()}</strong>
                                <span>{component.component_ref.clone()}</span>
                                <span>{component.description.clone().unwrap_or_default()}</span>
                            </div> }
                        }).collect_view()}
                    </div>
                    {book.plugin_id.clone().map(|plugin_id| {
                        let open_plugin = open_plugin.clone();
                        view! { <button class="primary" type="button" on:click=move |_| open_plugin(plugin_id.clone())>"Manage plugin →"</button> }
                    })}
                </div>
            }.into_any(),
            "AUTOBIOGRAPHY" => view! {
                <div class="book-detail-body">
                    <div class="index-table">
                        <div class="index-row"><span class="spine-cell">"REVISION"</span><span>{format!("rev {}", book.revision)}</span></div>
                    </div>
                    <pre>{book.body.clone().unwrap_or_default()}</pre>
                </div>
            }.into_any(),
            _ => view! {
                <div class="book-detail-body">
                    <div class="index-table">
                        <div class="index-row"><span class="spine-cell">"KIND"</span><span>{kind.clone()}</span></div>
                        <div class="index-row"><span class="spine-cell">"REVISION"</span><span>{format!("rev {}", book.revision)}</span></div>
                    </div>
                    <pre>{book.body.clone().unwrap_or_default()}</pre>
                </div>
            }.into_any(),
        }}
    }
}

#[component]
fn PluginDetailView(
    plugin_id: String,
    on_back: Arc<dyn Fn() + Send + Sync + 'static>,
    on_upgrade_done: Arc<dyn Fn() + Send + Sync + 'static>,
    on_changed: Arc<dyn Fn() + Send + Sync + 'static>,
) -> impl IntoView {
    let detail = RwSignal::new(None::<PluginDetail>);
    let status = RwSignal::new(String::new());
    let loading = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let upgrade_open = RwSignal::new(false);
    let upgrade_version = RwSignal::new(String::new());
    let upgrade_diff = RwSignal::new(None::<UpgradeDiff>);
    let upgrade_status = RwSignal::new(String::new());
    let upgrade_busy = RwSignal::new(false);
    let uninstall_armed = RwSignal::new(false);

    let load = {
        let plugin_id = plugin_id.clone();
        move |_| {
            loading.set(true);
            let plugin_id = plugin_id.clone();
            let detail = detail;
            let status = status;
            let loading = loading;
            spawn_local(async move {
                match Request::get(&format!("/api/v1/plugins/{plugin_id}"))
                    .send()
                    .await
                {
                    Ok(response) if response.ok() => match response.json::<PluginDetail>().await {
                        Ok(found) => {
                            detail.set(Some(found));
                            status.set(String::new());
                        }
                        Err(_) => status.set("Plugin response was not valid.".into()),
                    },
                    Ok(response) => status.set(format!(
                        "Plugin request failed: {}",
                        api_error(&response).await
                    )),
                    Err(_) => status.set("Plugin service did not answer.".into()),
                }
                loading.set(false);
            });
        }
    };
    load(());

    let set_state = {
        let on_changed = on_changed.clone();
        let load = load.clone();
        move |next_state: &'static str| {
            let Some(current) = detail.get_untracked() else {
                return;
            };
            let plugin_id = current.id.clone();
            busy.set(true);
            status.set(if next_state == "enabled" {
                "Enabling...".into()
            } else {
                "Disabling...".into()
            });
            let on_changed = on_changed.clone();
            let load = load.clone();
            spawn_local(async move {
                let request = Request::patch(&format!("/api/v1/plugins/{plugin_id}")).json(
                    &PatchPluginRequest {
                        state: Some(next_state),
                        trust: None,
                    },
                );
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            on_changed();
                            if next_state == "enabled" {
                                "Plugin enabled.".into()
                            } else {
                                "Plugin disabled.".into()
                            }
                        }
                        Ok(response) => {
                            format!("Update rejected: {}", api_error(&response).await)
                        }
                        Err(_) => "Plugin service did not answer.".into(),
                    },
                    Err(_) => "The update request could not be encoded.".into(),
                };
                status.set(outcome);
                busy.set(false);
                load(());
            });
        }
    };

    let start_upgrade = {
        move |_| {
            upgrade_open.set(true);
            upgrade_diff.set(None);
            upgrade_status.set(String::new());
            upgrade_version.set(String::new());
        }
    };
    let cancel_upgrade = {
        move |_: leptos::ev::MouseEvent| {
            upgrade_open.set(false);
            upgrade_diff.set(None);
        }
    };
    let stage_upgrade = {
        move |event: leptos::ev::SubmitEvent| {
            event.prevent_default();
            let version = upgrade_version.get_untracked().trim().to_owned();
            if version.is_empty() {
                upgrade_status.set("Enter the target version.".into());
                return;
            }
            let Some(current) = detail.get_untracked() else {
                return;
            };
            let plugin_id = current.id.clone();
            upgrade_busy.set(true);
            upgrade_status.set("Staging upgrade and computing the diff...".into());
            let upgrade_diff = upgrade_diff;
            let upgrade_status = upgrade_status;
            let upgrade_busy = upgrade_busy;
            spawn_local(async move {
                let request = Request::post(&format!("/api/v1/plugins/{plugin_id}/upgrade"))
                    .json(&PluginUpgradeRequest { version: &version });
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => match response.json::<UpgradeDiff>().await
                        {
                            Ok(diff) => {
                                upgrade_diff.set(Some(diff));
                                Ok("Review the diff below before activating.".to_owned())
                            }
                            Err(_) => Err("The upgrade response was not valid.".to_owned()),
                        },
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Plugin service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The upgrade request could not be encoded.".to_owned()),
                };
                upgrade_busy.set(false);
                match outcome {
                    Ok(message) => upgrade_status.set(message),
                    Err(error) => upgrade_status.set(format!("Upgrade staging failed: {error}")),
                }
            });
        }
    };
    let activate_upgrade = {
        let on_upgrade_done = on_upgrade_done.clone();
        let load = load.clone();
        move |_| {
            let Some(current) = detail.get_untracked() else {
                return;
            };
            let Some(diff) = upgrade_diff.get_untracked() else {
                return;
            };
            let plugin_id = current.id.clone();
            let version = diff.new_version.clone();
            upgrade_busy.set(true);
            upgrade_status.set("Running self-test and activating...".into());
            let on_upgrade_done = on_upgrade_done.clone();
            let load = load.clone();
            let upgrade_open = upgrade_open;
            let upgrade_diff = upgrade_diff;
            spawn_local(async move {
                let request = Request::post(&format!(
                    "/api/v1/plugins/{plugin_id}/upgrade/{version}/activate"
                ))
                .send();
                let outcome = match request.await {
                    Ok(response) if response.ok() => {
                        on_upgrade_done();
                        upgrade_open.set(false);
                        upgrade_diff.set(None);
                        load(());
                        Ok("Upgrade activated.".to_owned())
                    }
                    Ok(response) => Err(api_error(&response).await),
                    Err(_) => Err("Plugin service did not answer.".to_owned()),
                };
                upgrade_busy.set(false);
                match outcome {
                    Ok(message) => upgrade_status.set(message),
                    Err(error) => upgrade_status.set(format!("Activation failed: {error}")),
                }
            });
        }
    };
    let rollback = {
        let on_changed = on_changed.clone();
        let load = load.clone();
        move |_| {
            let Some(current) = detail.get_untracked() else {
                return;
            };
            let plugin_id = current.id.clone();
            busy.set(true);
            status.set("Rolling back to the previous version...".into());
            let on_changed = on_changed.clone();
            let load = load.clone();
            spawn_local(async move {
                let request = Request::post(&format!("/api/v1/plugins/{plugin_id}/rollback"));
                let outcome = match request.send().await {
                    Ok(response) if response.ok() => {
                        on_changed();
                        Ok("Rolled back to the previous version.".to_owned())
                    }
                    Ok(response) => Err(api_error(&response).await),
                    Err(_) => Err("Plugin service did not answer.".to_owned()),
                };
                busy.set(false);
                match outcome {
                    Ok(message) => status.set(message),
                    Err(error) => status.set(format!("Rollback failed: {error}")),
                }
                load(());
            });
        }
    };
    let uninstall = {
        let on_changed = on_changed.clone();
        let on_back = on_back.clone();
        move |_| {
            if !uninstall_armed.get_untracked() {
                uninstall_armed.set(true);
                status.set("Click Uninstall again to confirm removal.".into());
                return;
            }
            let Some(current) = detail.get_untracked() else {
                return;
            };
            let plugin_id = current.id.clone();
            busy.set(true);
            status.set("Uninstalling...".into());
            let on_changed = on_changed.clone();
            let on_back = on_back.clone();
            spawn_local(async move {
                let outcome = match Request::delete(&format!("/api/v1/plugins/{plugin_id}"))
                    .send()
                    .await
                {
                    Ok(response) if response.ok() => {
                        on_changed();
                        on_back();
                        Ok(())
                    }
                    Ok(response) => Err(api_error(&response).await),
                    Err(_) => Err("Plugin service did not answer.".to_owned()),
                };
                if let Err(error) = outcome {
                    status.set(format!("Uninstall failed: {error}"));
                    busy.set(false);
                }
            });
        }
    };

    view! {
        <div class="page-heading">
            <div><p class="utility">"CONNECT / PLUGIN"</p><h1>{move || detail.get().map_or_else(|| "Plugin".into(), |plugin| plugin.name.clone())}</h1></div>
            <button class="text-button" on:click=move |_| on_back()>"← Back to Library"</button>
        </div>
        <p class="form-note">{move || status.get()}</p>
        {move || if loading.get() {
            view! { <div class="stepper-loading"><p>"Loading plugin detail..."</p></div> }.into_any()
        } else if let Some(plugin) = detail.get() {
            let plugin = plugin.clone();
            let set_state = set_state.clone();
            let rollback = rollback.clone();
            let uninstall = uninstall.clone();
            let activate_upgrade = activate_upgrade.clone();
            let has_previous = plugin.installations.iter().filter(|install| install.status == "active").count() >= 2;
            view! {
                <div class="manifest-preview">
                    <div class="manifest-head">
                        <div>
                            <h3>{format!("{} · {}", plugin.name.clone(), plugin.version.clone())}</h3>
                            <p>{plugin.description.clone()}</p>
                        </div>
                        <span class=format!("state-badge {}", plugin.state.to_lowercase())>{plugin.state.to_uppercase()}</span>
                    </div>
                    <div class="index-table">
                        <div class="index-row"><span class="spine-cell">"ID"</span><span>{plugin.id.clone()}</span></div>
                        <div class="index-row"><span class="spine-cell">"PUBLISHER"</span><span>{plugin.publisher.clone().unwrap_or_else(|| "—".into())}</span></div>
                        <div class="index-row"><span class="spine-cell">"TRUST"</span><span>{plugin.trust.clone()}{if plugin.verified { " · VERIFIED SIGNATURE" } else { "" }}</span></div>
                        <div class="index-row"><span class="spine-cell">"SOURCE"</span><span>{plugin.source_uri.clone()}</span></div>
                        <div class="index-row"><span class="spine-cell">"COMMIT"</span><span class="mono-break">{plugin.commit_sha.clone().unwrap_or_else(|| "—".into())}</span></div>
                        <div class="index-row"><span class="spine-cell">"DIGEST"</span><span class="mono-break">{plugin.artifact_digest.clone().unwrap_or_else(|| "—".into())}</span></div>
                        <div class="index-row"><span class="spine-cell">"NETWORK"</span><span>{plugin.network_policy.clone()}</span></div>
                    </div>
                    <p class="utility">"COMPONENTS"</p>
                    <div class="index-table">
                        {plugin.components.iter().map(|component| {
                            view! { <div class="index-row component-row">
                                <span class="spine-cell">{component.component_type.to_uppercase()}</span>
                                <strong>{component.name.clone()}</strong>
                                <span>{component.manifest_ref.clone()}</span>
                                <span>{component.metadata.get("description").and_then(|v| v.as_str()).unwrap_or_default().to_owned()}</span>
                            </div> }
                        }).collect_view()}
                    </div>
                    <p class="utility">"PERMISSIONS (REFERENCES ONLY)"</p>
                    <div class="index-table">
                        {plugin.permissions.iter().map(|permission| {
                            view! { <div class="index-row component-row">
                                <span class="spine-cell">{permission.domain.to_uppercase()}</span>
                                <span>{permission.scope_value.clone()}</span>
                                <span></span><span></span>
                            </div> }
                        }).collect_view()}
                    </div>
                    <p class="utility">"INSTALLATION HISTORY"</p>
                    <div class="index-table">
                        <div class="index-row header" role="row"><span>"VERSION"</span><span>"STATUS"</span><span>"INSTALLED"</span><span>"ACTIVATED"</span></div>
                        {plugin.installations.iter().map(|install| {
                            view! { <div class="index-row component-row">
                                <strong>{install.version.clone()}</strong>
                                <span>{install.status.to_uppercase()}</span>
                                <span>{install.installed_at.clone().unwrap_or_else(|| "—".into())}</span>
                                <span>{install.activated_at.clone().unwrap_or_else(|| "—".into())}</span>
                            </div> }
                        }).collect_view()}
                    </div>
                    <div class="stepper-actions">
                        {if plugin.state == "enabled" {
                            view! { <button class="secondary" type="button" disabled=move || busy.get() on:click=move |_| set_state("dormant")>"Disable"</button> }.into_any()
                        } else {
                            view! { <button class="primary" type="button" disabled=move || busy.get() on:click=move |_| set_state("enabled")>"Enable"</button> }.into_any()
                        }}
                        <button class="secondary" type="button" on:click=start_upgrade>"Upgrade"</button>
                        <button class="secondary" type="button" disabled=move || busy.get() || !has_previous on:click=rollback>"Rollback"</button>
                        <button class="secondary danger" type="button" disabled=move || busy.get() on:click=uninstall>
                            {move || if uninstall_armed.get() { "Confirm uninstall" } else { "Uninstall" }}
                        </button>
                    </div>
                </div>
                {move || upgrade_open.get().then(|| {
                    let activate_upgrade = activate_upgrade.clone();
                    view! {
                        <section class="model-section">
                            <div class="section-heading">
                                <div><p class="utility">"PLUGIN / UPGRADE"</p><h2>"Staged upgrade"</h2></div>
                                <button class="text-button" on:click=cancel_upgrade>"Close"</button>
                            </div>
                            {move || upgrade_diff.get().map_or_else(
                                || view! {
                                    <form class="filter-row" on:submit=stage_upgrade>
                                        <input required maxlength="64" placeholder="Target version (e.g. 1.4.0)" prop:value=move || upgrade_version.get()
                                            on:input=move |event| upgrade_version.set(event_target_value(&event)) />
                                        <button type="submit" class="primary" disabled=move || upgrade_busy.get()>
                                            {move || if upgrade_busy.get() { "Staging..." } else { "Stage upgrade" }}
                                        </button>
                                    </form>
                                }.into_any(),
                                |diff| {
                                    let diff = diff.clone();
                                    let activate_upgrade = activate_upgrade.clone();
                                    view! {
                                        <div class="upgrade-diff">
                                            <div class="index-table">
                                                <div class="index-row"><span class="spine-cell">"CURRENT"</span><span>{diff.current_version.clone()}</span></div>
                                                <div class="index-row"><span class="spine-cell">"TARGET"</span><span>{diff.new_version.clone()}</span></div>
                                                <div class="index-row"><span class="spine-cell">"DIGEST"</span><span class="mono-break">{diff.artifact_digest.clone()}</span></div>
                                                <div class="index-row"><span class="spine-cell">"COMMIT"</span><span class="mono-break">{diff.commit_sha.clone().unwrap_or_else(|| "—".into())}</span></div>
                                            </div>
                                            <p class="utility">"PERMISSION DIFF"</p>
                                            <div class="diff-grid">
                                                <div><strong class="diff-added">"ADDED"</strong>
                                                    {diff.permissions.added.iter().map(|permission| view! { <div class="diff-line">{format!("{} → {}", permission.domain.to_uppercase(), permission.scope_value)}</div> }).collect_view()}
                                                </div>
                                                <div><strong class="diff-removed">"REMOVED"</strong>
                                                    {diff.permissions.removed.iter().map(|permission| view! { <div class="diff-line">{format!("{} → {}", permission.domain.to_uppercase(), permission.scope_value)}</div> }).collect_view()}
                                                </div>
                                            </div>
                                            <p class="utility">"CAPABILITY DIFF"</p>
                                            <div class="diff-grid">
                                                <div><strong class="diff-added">"ADDED"</strong>
                                                    {diff.capabilities.added.iter().map(|name| view! { <div class="diff-line">{name.clone()}</div> }).collect_view()}
                                                </div>
                                                <div><strong class="diff-removed">"REMOVED"</strong>
                                                    {diff.capabilities.removed.iter().map(|name| view! { <div class="diff-line">{name.clone()}</div> }).collect_view()}
                                                </div>
                                            </div>
                                            <div class="stepper-actions">
                                                <button class="secondary" type="button" on:click=cancel_upgrade>"Cancel"</button>
                                                <button class="primary" type="button" disabled=move || upgrade_busy.get() on:click=activate_upgrade>
                                                    {move || if upgrade_busy.get() { "Activating..." } else { "Activate upgrade" }}
                                                </button>
                                            </div>
                                        </div>
                                    }.into_any()
                                }
                            )}
                            <p class="form-note">{move || upgrade_status.get()}</p>
                        </section>
                    }.into_any()
                })}
            }.into_any()
        } else {
            view! { <div class="operational-empty"><p>"Plugin not found or no longer visible."</p></div> }.into_any()
        }}
    }
}

// ---------------------------------------------------------------------------
// Lane F: sandbox terminal page helpers
// ---------------------------------------------------------------------------

fn load_open_terminal() -> Option<OpenTerminalSession> {
    let storage = web_sys::window()?.session_storage().ok()??;
    let value = storage.get_item(OPEN_TERMINAL_KEY).ok()??;
    serde_json::from_str(&value).ok()
}

fn store_open_terminal(session: &OpenTerminalSession) {
    let Some(storage) =
        web_sys::window().and_then(|window| window.session_storage().ok().flatten())
    else {
        return;
    };
    if let Ok(value) = serde_json::to_string(session) {
        let _ = storage.set_item(OPEN_TERMINAL_KEY, &value);
    }
}

fn clear_open_terminal() {
    let Some(storage) =
        web_sys::window().and_then(|window| window.session_storage().ok().flatten())
    else {
        return;
    };
    let _ = storage.remove_item(OPEN_TERMINAL_KEY);
}

/// Encodes UTF-8 bytes as base64 via the browser's `btoa` (each byte maps to
/// one Latin-1 char code).
fn base64_encode_utf8(text: &str) -> Result<String, ()> {
    let binary = text
        .as_bytes()
        .iter()
        .map(|byte| *byte as char)
        .collect::<String>();
    web_sys::window().ok_or(())?.btoa(&binary).map_err(|_| ())
}

/// Decodes base64 (browser `atob`) back into UTF-8 text.
fn base64_decode_utf8(encoded: &str) -> Result<String, ()> {
    let binary = web_sys::window().ok_or(())?.atob(encoded).map_err(|_| ())?;
    let bytes = binary.chars().map(|ch| ch as u8).collect::<Vec<u8>>();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn terminal_ended(state: &str) -> bool {
    matches!(state, "Exited" | "Terminated" | "Unrecoverable" | "Lost")
}

fn join_sandbox_path(parent: &str, name: &str) -> String {
    let trimmed = parent.trim_end_matches('/');
    if trimmed.is_empty() {
        name.to_owned()
    } else {
        format!("{trimmed}/{name}")
    }
}

#[component]
fn TerminalsPage() -> impl IntoView {
    let lifecycle = Arc::new(AtomicBool::new(true));
    on_cleanup({
        let lifecycle = Arc::clone(&lifecycle);
        move || lifecycle.store(false, Ordering::Release)
    });
    let workspaces = RwSignal::new(Vec::<WorkspaceSummary>::new());
    let status = RwSignal::new(String::new());
    let selected_workspace = RwSignal::new(String::new());
    let start_command = RwSignal::new("/bin/sh".to_owned());
    let starting = RwSignal::new(false);
    let session = RwSignal::new(None::<OpenTerminalSession>);
    let output = RwSignal::new(String::new());
    let cursor = RwSignal::new(0_u64);
    let term_state = RwSignal::new(String::new());
    let term_started = RwSignal::new(false);
    let input_line = RwSignal::new(String::new());
    let sending = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let cols = RwSignal::new(80_u16);
    let rows = RwSignal::new(24_u16);
    let term_generation = RwSignal::new(0_u64);
    let reconnect_target = RwSignal::new(load_open_terminal());
    let output_ref = NodeRef::<leptos::html::Pre>::new();
    // file manager
    let fm_path = RwSignal::new(".".to_owned());
    let fm_entries = RwSignal::new(Vec::<FsEntry>::new());
    let fm_status = RwSignal::new(String::new());
    let fm_content = RwSignal::new(None::<(String, String)>);
    let fm_loading = RwSignal::new(false);
    let fm_file_name = RwSignal::new(String::new());
    let fm_file_body = RwSignal::new(String::new());
    let fm_write_status = RwSignal::new(String::new());
    let fm_writing = RwSignal::new(false);

    load_workspaces(workspaces);
    // Auto-select the first workspace when the list arrives.
    Effect::new(move |_| {
        if selected_workspace.get_untracked().is_empty()
            && let Some(first) = workspaces.get().first()
        {
            selected_workspace.set(first.id.clone());
        }
    });
    // Auto-scroll the terminal output to the bottom on new output.
    Effect::new(move |_| {
        let _ = output.get();
        if let Some(element) = output_ref.get() {
            element.set_scroll_top(element.scroll_height());
        }
    });

    let start_terminal = {
        let lifecycle = Arc::clone(&lifecycle);
        move |workspace_id: String, command: Vec<String>| {
            starting.set(true);
            status.set("Starting sandbox terminal...".into());
            let session_gen = term_generation.get_untracked().wrapping_add(1);
            term_generation.set(session_gen);
            let lifecycle = Arc::clone(&lifecycle);
            let session = session;
            let output = output;
            let cursor = cursor;
            let term_state = term_state;
            let term_started = term_started;
            let status = status;
            let starting = starting;
            let term_generation = term_generation;
            let reconnect_target = reconnect_target;
            let cols = cols;
            let rows = rows;
            let fm_path = fm_path;
            let fm_entries = fm_entries;
            let fm_status = fm_status;
            spawn_local(async move {
                let body = TerminalStartBody {
                    workspace_id: &workspace_id,
                    command: command.iter().map(String::as_str).collect(),
                    cols: Some(cols.get_untracked()),
                    rows: Some(rows.get_untracked()),
                };
                let request = Request::post("/api/v1/sandbox/terminal/start").json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<TerminalStartResponse>().await {
                                Ok(started) => Ok(started),
                                Err(_) => Err("The terminal response was not valid.".to_owned()),
                            }
                        }
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The start request could not be encoded.".to_owned()),
                };
                if !lifecycle_is_active(&lifecycle) {
                    return;
                }
                if term_generation.get_untracked() != session_gen {
                    return;
                }
                let started = match outcome {
                    Ok(started) => started,
                    Err(error) => {
                        status.set(format!("Could not start terminal: {error}"));
                        starting.set(false);
                        return;
                    }
                };
                let active = OpenTerminalSession {
                    workspace_id: workspace_id.clone(),
                    terminal_id: started.terminal_id.clone(),
                };
                store_open_terminal(&active);
                reconnect_target.set(None);
                session.set(Some(active));
                output.set(String::new());
                cursor.set(0_u64);
                term_state.set("Running".into());
                term_started.set(true);
                starting.set(false);
                status.set(format!(
                    "Terminal ready · network {}",
                    started.network_policy
                ));
                fm_path.set(".".into());
                fm_entries.set(Vec::new());
                fm_status.set("List the workspace files below.".into());
                // Poll loop: read every second while the page is visible.
                loop {
                    if !lifecycle_is_active(&lifecycle) {
                        return;
                    }
                    if term_generation.get_untracked() != session_gen {
                        return;
                    }
                    let Some(current) = session.get() else {
                        return;
                    };
                    if current.terminal_id != started.terminal_id {
                        return;
                    }
                    let read_body = TerminalReadBody {
                        workspace_id: &current.workspace_id,
                        after_cursor: cursor.get_untracked(),
                        max_bytes: TERMINAL_MAX_READ_BYTES,
                    };
                    let request = Request::post(&format!(
                        "/api/v1/sandbox/terminal/{}/read",
                        current.terminal_id
                    ))
                    .json(&read_body);
                    let read = match request {
                        Ok(request) => request.send().await,
                        Err(_) => break,
                    };
                    let read = match read {
                        Ok(response) if response.ok() => {
                            match response.json::<TerminalReadResponse>().await {
                                Ok(found) => found,
                                Err(_) => {
                                    status.set("Terminal read response was not valid.".into());
                                    continue;
                                }
                            }
                        }
                        Ok(response) => {
                            if matches!(response.status(), 401 | 403 | 404) {
                                status.set(format!(
                                    "Terminal access was lost: {}",
                                    api_error(&response).await
                                ));
                                break;
                            }
                            status.set(format!(
                                "Terminal read failed: {}",
                                api_error(&response).await
                            ));
                            continue;
                        }
                        Err(_) => {
                            status.set("Sandbox service did not answer.".into());
                            continue;
                        }
                    };
                    if let Ok(text) = base64_decode_utf8(&read.data_base64)
                        && !text.is_empty()
                    {
                        output.update(|current| current.push_str(&text));
                    }
                    cursor.set(read.next_cursor);
                    term_state.set(read.state.clone());
                    // `output_complete` means all buffered output was returned,
                    // NOT that the terminal ended — only a terminal state stops
                    // the poll loop (the daemon keeps the session running).
                    if terminal_ended(&read.state) {
                        status.set(format!("Terminal {}", read.state.to_uppercase()));
                        break;
                    }
                    wait_for_poll(TERMINAL_POLL_MS).await;
                }
            });
        }
    };

    let reconnect = {
        let start_terminal = start_terminal.clone();
        move |target: OpenTerminalSession| {
            let old = target.clone();
            let command = start_command
                .get_untracked()
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<String>>();
            let workspace_id = old.workspace_id.clone();
            status.set("Reconnecting to a fresh session in the same workspace...".into());
            let start_terminal = start_terminal.clone();
            spawn_local(async move {
                // Best-effort close of the previous daemon session.
                if let Ok(request) = Request::post(&format!(
                    "/api/v1/sandbox/terminal/{}/close",
                    old.terminal_id
                ))
                .json(&WorkspaceIdBody {
                    workspace_id: &old.workspace_id,
                }) {
                    let _ = request.send().await;
                }
                start_terminal(workspace_id, command);
            });
        }
    };
    let dismiss_reconnect = move |_| reconnect_target.set(None);

    let send_line = {
        move |event: leptos::ev::SubmitEvent| {
            event.prevent_default();
            let Some(current) = session.get_untracked() else {
                return;
            };
            let line = input_line.get_untracked();
            if line.is_empty() {
                return;
            }
            sending.set(true);
            let Ok(encoded) = base64_encode_utf8(&format!("{line}\n")) else {
                status.set("Browser base64 encoding is unavailable.".into());
                sending.set(false);
                return;
            };
            let input_line = input_line;
            let sending = sending;
            let status = status;
            spawn_local(async move {
                let body = TerminalWriteBody {
                    workspace_id: &current.workspace_id,
                    data_base64: &encoded,
                };
                let request = Request::post(&format!(
                    "/api/v1/sandbox/terminal/{}/write",
                    current.terminal_id
                ))
                .json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => Ok(()),
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The write request could not be encoded.".to_owned()),
                };
                if let Err(error) = outcome {
                    status.set(format!("Write failed: {error}"));
                } else {
                    input_line.set(String::new());
                }
                sending.set(false);
            });
        }
    };
    let apply_resize = {
        move |_| {
            let Some(current) = session.get_untracked() else {
                return;
            };
            busy.set(true);
            status.set("Resizing terminal...".into());
            let busy = busy;
            let status = status;
            spawn_local(async move {
                let body = TerminalResizeBody {
                    workspace_id: &current.workspace_id,
                    cols: cols.get_untracked(),
                    rows: rows.get_untracked(),
                };
                let request = Request::post(&format!(
                    "/api/v1/sandbox/terminal/{}/resize",
                    current.terminal_id
                ))
                .json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => Ok("Terminal resized.".to_owned()),
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The resize request could not be encoded.".to_owned()),
                };
                match outcome {
                    Ok(message) => status.set(message),
                    Err(error) => status.set(format!("Resize failed: {error}")),
                }
                busy.set(false);
            });
        }
    };
    let interrupt = {
        move |_| {
            let Some(current) = session.get_untracked() else {
                return;
            };
            busy.set(true);
            status.set("Interrupting (SIGINT)...".into());
            let busy = busy;
            let status = status;
            spawn_local(async move {
                let body = WorkspaceIdBody {
                    workspace_id: &current.workspace_id,
                };
                let request = Request::post(&format!(
                    "/api/v1/sandbox/terminal/{}/interrupt",
                    current.terminal_id
                ))
                .json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => Ok("Interrupt sent.".to_owned()),
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The interrupt request could not be encoded.".to_owned()),
                };
                match outcome {
                    Ok(message) => status.set(message),
                    Err(error) => status.set(format!("Interrupt failed: {error}")),
                }
                busy.set(false);
            });
        }
    };
    let close_terminal = {
        move |_| {
            let Some(current) = session.get_untracked() else {
                return;
            };
            busy.set(true);
            status.set("Closing terminal...".into());
            let session = session;
            let busy = busy;
            let status = status;
            let term_generation = term_generation;
            let term_started = term_started;
            let output = output;
            spawn_local(async move {
                let body = WorkspaceIdBody {
                    workspace_id: &current.workspace_id,
                };
                let request = Request::post(&format!(
                    "/api/v1/sandbox/terminal/{}/close",
                    current.terminal_id
                ))
                .json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => Ok(()),
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The close request could not be encoded.".to_owned()),
                };
                match outcome {
                    Ok(()) => {
                        clear_open_terminal();
                        term_generation.update(|value| *value = value.wrapping_add(1));
                        session.set(None);
                        term_started.set(false);
                        output.set(String::new());
                        status.set("Terminal closed.".into());
                    }
                    Err(error) => status.set(format!("Close failed: {error}")),
                }
                busy.set(false);
            });
        }
    };

    // ---- file manager ----
    let fm_list = {
        move |path: Option<String>| {
            let Some(workspace_id) =
                session
                    .get()
                    .map(|current| current.workspace_id)
                    .or_else(|| {
                        let value = selected_workspace.get_untracked();
                        (!value.is_empty()).then_some(value)
                    })
            else {
                fm_status.set("Select a workspace first.".into());
                return;
            };
            if let Some(path) = path {
                fm_path.set(path.clone());
            }
            let path = fm_path.get_untracked();
            fm_loading.set(true);
            fm_status.set("Listing files...".into());
            let fm_path = fm_path;
            let fm_entries = fm_entries;
            let fm_status = fm_status;
            let fm_loading = fm_loading;
            spawn_local(async move {
                let body = SandboxPathBody {
                    workspace_id: &workspace_id,
                    path: &path,
                };
                let request = Request::post("/api/v1/sandbox/files/list").json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<FileListResponse>().await {
                                Ok(found) => {
                                    fm_entries.set(found.entries);
                                    fm_path.set(path);
                                    Ok("".to_owned())
                                }
                                Err(_) => Err("The listing response was not valid.".to_owned()),
                            }
                        }
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The listing request could not be encoded.".to_owned()),
                };
                fm_loading.set(false);
                match outcome {
                    Ok(message) => {
                        if !message.is_empty() {
                            fm_status.set(message);
                        }
                    }
                    Err(error) => fm_status.set(format!("Listing failed: {error}")),
                }
            });
        }
    };
    let fm_open = {
        move |entry: FsEntry| {
            if entry.kind == "Directory" {
                fm_content.set(None);
                let path = join_sandbox_path(&fm_path.get_untracked(), &entry.name);
                fm_list(Some(path));
                return;
            }
            let Some(workspace_id) =
                session
                    .get()
                    .map(|current| current.workspace_id)
                    .or_else(|| {
                        let value = selected_workspace.get_untracked();
                        (!value.is_empty()).then_some(value)
                    })
            else {
                fm_status.set("Select a workspace first.".into());
                return;
            };
            let path = join_sandbox_path(&fm_path.get_untracked(), &entry.name);
            fm_status.set(format!("Reading {}", path));
            let fm_content = fm_content;
            let fm_status = fm_status;
            spawn_local(async move {
                let body = SandboxPathBody {
                    workspace_id: &workspace_id,
                    path: &path,
                };
                let request = Request::post("/api/v1/sandbox/files/read").json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<ReadFileResponse>().await {
                                Ok(found) => match base64_decode_utf8(&found.data_base64) {
                                    Ok(text) => {
                                        fm_content.set(Some((path.clone(), text)));
                                        Ok("".to_owned())
                                    }
                                    Err(_) => {
                                        Err("The file content could not be decoded.".to_owned())
                                    }
                                },
                                Err(_) => Err("The file response was not valid.".to_owned()),
                            }
                        }
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The read request could not be encoded.".to_owned()),
                };
                match outcome {
                    Ok(message) => {
                        if !message.is_empty() {
                            fm_status.set(message);
                        }
                    }
                    Err(error) => fm_status.set(format!("Read failed: {error}")),
                }
            });
        }
    };
    let fm_delete = {
        move |entry: FsEntry| {
            let Some(workspace_id) =
                session
                    .get()
                    .map(|current| current.workspace_id)
                    .or_else(|| {
                        let value = selected_workspace.get_untracked();
                        (!value.is_empty()).then_some(value)
                    })
            else {
                fm_status.set("Select a workspace first.".into());
                return;
            };
            let path = join_sandbox_path(&fm_path.get_untracked(), &entry.name);
            let confirmed = web_sys::window().is_some_and(|window| {
                window
                    .confirm_with_message(&format!("Delete {path}?"))
                    .unwrap_or(false)
            });
            if !confirmed {
                return;
            }
            fm_status.set(format!("Deleting {}", path));
            let fm_status = fm_status;
            let fm_list = fm_list;
            spawn_local(async move {
                let body = SandboxPathBody {
                    workspace_id: &workspace_id,
                    path: &path,
                };
                let request = Request::post("/api/v1/sandbox/files/remove").json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => Ok(()),
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The delete request could not be encoded.".to_owned()),
                };
                match outcome {
                    Ok(()) => {
                        fm_status.set(format!("Deleted {path}."));
                        fm_list(None);
                    }
                    Err(error) => fm_status.set(format!("Delete failed: {error}")),
                }
            });
        }
    };
    let fm_write = {
        move |event: leptos::ev::SubmitEvent| {
            event.prevent_default();
            let Some(workspace_id) =
                session
                    .get()
                    .map(|current| current.workspace_id)
                    .or_else(|| {
                        let value = selected_workspace.get_untracked();
                        (!value.is_empty()).then_some(value)
                    })
            else {
                fm_write_status.set("Select a workspace first.".into());
                return;
            };
            let name = fm_file_name.get_untracked().trim().to_owned();
            if name.is_empty() {
                fm_write_status.set("Enter a file name.".into());
                return;
            }
            let body_text = fm_file_body.get_untracked();
            let Ok(encoded) = base64_encode_utf8(&body_text) else {
                fm_write_status.set("Browser base64 encoding is unavailable.".into());
                return;
            };
            let path = join_sandbox_path(&fm_path.get_untracked(), &name);
            fm_writing.set(true);
            fm_write_status.set("Writing file...".into());
            let fm_write_status = fm_write_status;
            let fm_writing = fm_writing;
            let fm_file_name = fm_file_name;
            let fm_file_body = fm_file_body;
            let fm_list = fm_list;
            spawn_local(async move {
                let body = SandboxWriteBody {
                    workspace_id: &workspace_id,
                    path: &path,
                    data_base64: &encoded,
                };
                let request = Request::post("/api/v1/sandbox/files/write").json(&body);
                let outcome = match request {
                    Ok(request) => match request.send().await {
                        Ok(response) if response.ok() => Ok(()),
                        Ok(response) => Err(api_error(&response).await),
                        Err(_) => Err("Sandbox service did not answer.".to_owned()),
                    },
                    Err(_) => Err("The write request could not be encoded.".to_owned()),
                };
                fm_writing.set(false);
                match outcome {
                    Ok(()) => {
                        fm_file_name.set(String::new());
                        fm_file_body.set(String::new());
                        fm_write_status.set(format!("Wrote {path}."));
                        fm_list(None);
                    }
                    Err(error) => fm_write_status.set(format!("Write failed: {error}")),
                }
            });
        }
    };

    view! {
        <div class="page-heading">
            <div><p class="utility">"OPERATE / SANDBOX TERMINAL"</p><h1>"Terminals"</h1></div>
            <span class="utility">{move || format!("{} WORKSPACES", workspaces.get().len())}</span>
        </div>
        <p class="form-note">{move || status.get()}</p>
        {move || if reconnect_target.get().is_some() && session.get().is_none() {
            let target = reconnect_target.get().unwrap();
            let reconnect = reconnect.clone();
            let dismiss = dismiss_reconnect;
            view! {
                <div class="reconnect-banner">
                    <p>"A terminal session from a previous visit is still open in this workspace. Session ids are assigned server-side, so reconnecting starts a fresh session in the same workspace (the old session is closed)."</p>
                    <div>
                        <button class="secondary" type="button" on:click=move |_| reconnect(target.clone())>"Reconnect (fresh session)"</button>
                        <button class="text-button" type="button" on:click=dismiss>"Dismiss"</button>
                    </div>
                </div>
            }.into_any()
        } else {
            view! { <span></span> }.into_any()
        }}
        {move || if session.get().is_none() {
            let start_terminal = start_terminal.clone();
            view! {
                <form class="filter-row" on:submit=move |event: leptos::ev::SubmitEvent| {
                    event.prevent_default();
                    let workspace_id = selected_workspace.get_untracked();
                    if workspace_id.is_empty() {
                        status.set("Select a workspace first.".into());
                        return;
                    }
                    let command = start_command.get_untracked();
                    let parsed = command.split_whitespace().map(str::to_owned).collect::<Vec<String>>();
                    if parsed.is_empty() {
                        status.set("Enter a command such as /bin/sh.".into());
                        return;
                    }
                    start_terminal(workspace_id, parsed);
                }>
                    <select prop:value=move || selected_workspace.get()
                        on:change=move |event| selected_workspace.set(event_target_value(&event))>
                        {workspaces.get().into_iter().map(|ws| {
                            let id = ws.id.clone();
                            let title = ws.title.clone();
                            view! { <option value=id>{title}</option> }
                        }).collect_view()}
                    </select>
                    <input placeholder="Command (default /bin/sh)" prop:value=move || start_command.get()
                        on:input=move |event| start_command.set(event_target_value(&event)) />
                    <button class="primary" type="submit" disabled=move || starting.get()>
                        {move || if starting.get() { "Starting..." } else { "Start terminal" }}
                    </button>
                </form>
                <div class="terminal-empty">
                    <span class="index-spine">"TTY"</span>
                    <div><h2>"No terminal open"</h2><p>"Terminals run inside the configured sandbox for the selected workspace. Output streams here; commands echo through the same session."</p></div>
                </div>
            }.into_any()
        } else {
            let fm_list = fm_list;
            view! {
                <div class="terminal-layout">
                    <div class="terminal-pane">
                        <div class="terminal-toolbar">
                            <span class=move || format!("state-badge {}", term_state.get().to_lowercase())>{move || term_state.get().to_uppercase()}</span>
                            <label>"Resize"
                                <select prop:value=move || format!("{}x{}", cols.get(), rows.get())
                                    on:change=move |event| {
                                        let value = event_target_value(&event);
                                        let mut parts = value.split('x');
                                        if let (Some(c), Some(r)) = (parts.next(), parts.next())
                                            && let (Ok(c), Ok(r)) =
                                                (c.parse::<u16>(), r.parse::<u16>())
                                        {
                                            cols.set(c);
                                            rows.set(r);
                                        }
                                    }>
                                    <option value="80x24">"80 × 24"</option>
                                    <option value="120x40">"120 × 40"</option>
                                    <option value="160x50">"160 × 50"</option>
                                    <option value="200x60">"200 × 60"</option>
                                </select>
                            </label>
                            <button class="secondary" type="button" disabled=move || busy.get() on:click=apply_resize>"Resize"</button>
                            <button class="secondary" type="button" disabled=move || busy.get() on:click=interrupt>"Interrupt"</button>
                            <button class="secondary danger" type="button" disabled=move || busy.get() on:click=close_terminal>"Close"</button>
                        </div>
                        <pre class="terminal-output" node_ref=output_ref tabindex="0">{move || output.get()}</pre>
                        <form class="terminal-input-row" on:submit=send_line>
                            <textarea rows="2" placeholder="Type a command… Enter to send, Shift+Enter for newline" disabled=move || sending.get()
                                prop:value=move || input_line.get()
                                on:keydown=move |event: web_sys::KeyboardEvent| {
                                    if event.key() == "Enter" && !event.shift_key() {
                                        event.prevent_default();
                                        if let Some(target) = event.target()
                                            && let Ok(el) = target.dyn_into::<web_sys::HtmlTextAreaElement>()
                                            && let Some(form) = el.form() {
                                                let _ = form.request_submit();
                                        }
                                    }
                                }
                                on:input=move |event| input_line.set(event_target_value(&event))></textarea>
                            <button class="primary" type="submit" disabled=move || sending.get()>
                                {move || if sending.get() { "Sending..." } else { "Send" }}
                            </button>
                        </form>
                    </div>
                    <aside class="file-pane">
                        <div class="section-heading"><div><p class="utility">"WORKSPACE FILES"</p></div>
                            <button class="text-button" type="button" on:click=move |_| fm_list(None)>"Refresh"</button>
                        </div>
                        <p class="form-note">{move || fm_status.get()}</p>
                        <div class="file-nav">
                            <button class="text-button" type="button" on:click=move |_| fm_list(Some(".".into()))>"."</button>
                            <span class="mono-break">{move || fm_path.get()}</span>
                        </div>
                        <div class="index-table file-list">
                            {move || fm_entries.get().into_iter().map(|entry| {
                                let open_entry = entry.clone();
                                let delete_entry = entry.clone();
                                view! {
                                    <div class="index-row component-row">
                                        <span class="spine-cell">{if entry.kind == "Directory" { "DIR" } else { "FILE" }}</span>
                                        <button class="text-button" type="button" on:click=move |_| fm_open(open_entry.clone())>{entry.name.clone()}</button>
                                        <span>{entry.size}</span>
                                        <button class="text-button" type="button" on:click=move |_| fm_delete(delete_entry.clone())>"Delete"</button>
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                        {move || fm_content.get().map(|(path, text)| {
                            let (path, text) = (path.clone(), text.clone());
                            view! {
                                <details class="file-viewer" open>
                                    <summary>{path}</summary>
                                    <pre>{text}</pre>
                                </details>
                            }.into_any()
                        })}
                        <form class="file-write" on:submit=fm_write>
                            <p class="utility">"WRITE FILE"</p>
                            <input maxlength="500" placeholder="File name" prop:value=move || fm_file_name.get()
                                on:input=move |event| fm_file_name.set(event_target_value(&event)) />
                            <textarea rows="4" placeholder="File contents" prop:value=move || fm_file_body.get()
                                on:input=move |event| fm_file_body.set(event_target_value(&event))></textarea>
                            <button class="secondary" type="submit" disabled=move || fm_writing.get()>
                                {move || if fm_writing.get() { "Writing..." } else { "Write file" }}
                            </button>
                            <p class="form-note">{move || fm_write_status.get()}</p>
                        </form>
                    </aside>
                </div>
            }.into_any()
        }}
    }
}

#[component]
fn ModelsPage() -> impl IntoView {
    let lifecycle = Arc::new(AtomicBool::new(true));
    on_cleanup({
        let lifecycle = Arc::clone(&lifecycle);
        move || lifecycle.store(false, Ordering::Release)
    });
    let configurations = RwSignal::new(Vec::<EmbeddingConfiguration>::new());
    let chat_configurations = RwSignal::new(Vec::<ChatModelConfiguration>::new());
    let status = RwSignal::new(String::new());
    let chat_status = RwSignal::new(String::new());
    let base_url = RwSignal::new("http://127.0.0.1:11434".to_owned());
    let model = RwSignal::new("nomic-embed-text".to_owned());
    let dimensions = RwSignal::new("768".to_owned());
    let chat_provider = RwSignal::new("ollama".to_owned());
    let chat_base_url = RwSignal::new("http://127.0.0.1:11434".to_owned());
    let chat_model = RwSignal::new("llama3.2".to_owned());
    let chat_secret = RwSignal::new(String::new());
    let chat_context = RwSignal::new("8192".to_owned());
    let chat_output = RwSignal::new("1024".to_owned());
    let chat_fallbacks = RwSignal::new(String::new());
    let detected = RwSignal::new(Vec::<DetectedProviderInfo>::new());
    let detecting = RwSignal::new(false);
    let catalog = RwSignal::new(Vec::<CatalogProvider>::new());
    let catalog_models = RwSignal::new(Vec::<CatalogModel>::new());
    let usage = RwSignal::new(None::<UsageSummary>);
    let usage_window = RwSignal::new("30d".to_owned());
    let usage_status = RwSignal::new(String::new());
    let usage_lifecycle = Arc::clone(&lifecycle);
    load_usage(&usage, &usage_status, &usage_window, Arc::clone(&lifecycle));
    load_configurations(configurations, status);
    load_chat_models(chat_configurations, chat_status, Arc::clone(&lifecycle));
    let catalog_lifecycle = Arc::clone(&lifecycle);
    spawn_local(async move {
        let response = Request::get("/api/v1/providers/catalog").send().await;
        if !lifecycle_is_active(&catalog_lifecycle) {
            return;
        }
        match response {
            Ok(response) if response.ok() => {
                if !lifecycle_is_active(&catalog_lifecycle) {
                    return;
                }
                match response.json::<Vec<CatalogProvider>>().await {
                    Ok(list) => {
                        catalog.set(list.clone());
                        // Populate the model select for the initial provider so the
                        // required select has a matching option and the form can submit.
                        if let Some(provider) = list
                            .into_iter()
                            .find(|entry| entry.provider_type == chat_provider.get_untracked())
                        {
                            catalog_models.set(provider.models.clone());
                            chat_status.set(String::new());
                            if let Some(first) = provider.models.first() {
                                chat_model.set(first.reference.clone());
                                chat_context.set(first.context_window.to_string());
                                chat_output.set(first.output_limit.to_string());
                            }
                        } else {
                            chat_status
                                .set(format!("{} not in catalog", chat_provider.get_untracked()));
                        }
                    }
                    Err(_) => chat_status.set("Could not load provider catalog".into()),
                }
            }
            Ok(_) | Err(_) => chat_status.set("Could not load provider catalog".into()),
        }
    });
    let test_pending = RwSignal::new(false);
    let live_pending = RwSignal::new(false);
    let test_ok = RwSignal::new(Option::<bool>::None);
    let test_message = RwSignal::new(String::new());
    let live_generation = RwSignal::new(0_u32);
    let delete_target = RwSignal::new(Option::<DeleteTarget>::None);
    let do_live_fetch = {
        let catalog = catalog;
        let catalog_models = catalog_models;
        let chat_model = chat_model;
        let chat_context = chat_context;
        let chat_output = chat_output;
        let chat_status = chat_status;
        let live_pending = live_pending;
        let test_ok = test_ok;
        let test_message = test_message;
        let test_pending = test_pending;
        move |base: String, api_key: String, manual: bool| {
            let trimmed = base.trim().to_owned();
            if trimmed.is_empty() {
                return;
            }
            let _catalog_snapshot = catalog.get_untracked();
            if manual {
                test_pending.set(true);
                test_ok.set(None);
                test_message.set("Testing…".into());
            } else {
                live_pending.set(true);
            }
            spawn_local(async move {
                let payload = ProviderTestRequest {
                    base_url: &trimmed,
                    api_key: if api_key.trim().is_empty() {
                        None
                    } else {
                        Some(api_key.trim())
                    },
                };
                let request = Request::post("/api/v1/providers/test").json(&payload);
                let response = match request {
                    Ok(req) => req.send().await,
                    Err(_) => {
                        live_pending.set(false);
                        test_pending.set(false);
                        test_ok.set(Some(false));
                        test_message.set("Could not encode provider test request.".into());
                        return;
                    }
                };
                match response {
                    Ok(resp) if resp.ok() => match resp.json::<ProviderTestResponse>().await {
                        Ok(data) => {
                            live_pending.set(false);
                            test_pending.set(false);
                            if data.ok && !data.models.is_empty() {
                                catalog_models.set(data.models.clone());
                                if let Some(first) = data.models.first() {
                                    let current = chat_model.get_untracked();
                                    let still_valid =
                                        data.models.iter().any(|m| m.reference == current);
                                    if !still_valid {
                                        chat_model.set(first.reference.clone());
                                        if first.context_window > 0 {
                                            chat_context.set(first.context_window.to_string());
                                        }
                                        if first.output_limit > 0 {
                                            chat_output.set(first.output_limit.to_string());
                                        }
                                    }
                                }
                                test_ok.set(Some(true));
                                test_message.set(data.detail.clone());
                                chat_status.set(format!("Live models loaded — {}.", data.detail));
                            } else if data.ok {
                                test_ok.set(Some(true));
                                test_message.set(data.detail.clone());
                                chat_status.set(data.detail);
                            } else {
                                test_ok.set(Some(false));
                                test_message.set(data.detail.clone());
                                if !manual {
                                    chat_status.set(format!(
                                        "Live probe failed — using catalog. {}",
                                        data.detail
                                    ));
                                }
                            }
                        }
                        Err(_) => {
                            live_pending.set(false);
                            test_pending.set(false);
                            test_ok.set(Some(false));
                            test_message.set("Provider test response was not valid.".into());
                        }
                    },
                    Ok(resp) => {
                        live_pending.set(false);
                        test_pending.set(false);
                        let msg = api_error(&resp).await;
                        test_ok.set(Some(false));
                        test_message.set(msg.clone());
                        if !manual {
                            chat_status.set(format!("Live probe failed — using catalog. {msg}"));
                        }
                    }
                    Err(_) => {
                        live_pending.set(false);
                        test_pending.set(false);
                        test_ok.set(Some(false));
                        test_message.set("Provider service did not answer.".into());
                        if !manual {
                            chat_status.set(
                                "Live probe failed — using catalog. Provider did not answer."
                                    .into(),
                            );
                        }
                    }
                }
            });
        }
    };
    let do_live_fetch_manual = do_live_fetch.clone();
    let apply_preset = {
        let catalog_clone = catalog;
        let do_live_fetch = do_live_fetch.clone();
        move |preset: &'static str| {
            let (provider_type, base) = match preset {
                "openai" => ("openai", "https://api.openai.com/v1"),
                "anthropic" => ("anthropic", "https://api.anthropic.com"),
                "ollama" => ("ollama", "http://127.0.0.1:11434"),
                _ => ("custom", ""),
            };
            chat_provider.set(provider_type.to_owned());
            chat_base_url.set(base.to_owned());
            chat_secret.set(String::new());
            test_ok.set(None);
            test_message.set(String::new());
            if let Some(provider) = catalog_clone
                .get_untracked()
                .into_iter()
                .find(|e| e.provider_type == provider_type)
            {
                catalog_models.set(provider.models.clone());
                if let Some(first) = provider.models.first() {
                    chat_model.set(first.reference.clone());
                    chat_context.set(first.context_window.to_string());
                    chat_output.set(first.output_limit.to_string());
                }
            } else {
                catalog_models.set(Vec::new());
            }
            if !base.is_empty() {
                do_live_fetch(base.to_owned(), String::new(), false);
            }
        }
    };
    let do_test = move |_| {
        let base = chat_base_url.get_untracked();
        let key = chat_secret.get_untracked();
        if base.trim().is_empty() {
            test_ok.set(Some(false));
            test_message.set("Enter a Base URL first.".into());
            return;
        }
        do_live_fetch_manual(base, key, true);
    };
    let on_provider_select = {
        let do_live_fetch = do_live_fetch.clone();
        move |event: leptos::ev::Event| {
            let provider_type = event_target_value(&event);
            chat_provider.set(provider_type.clone());
            test_ok.set(None);
            test_message.set(String::new());
            if let Some(provider) = catalog
                .get_untracked()
                .into_iter()
                .find(|entry| entry.provider_type == provider_type)
            {
                chat_base_url.set(provider.base_url.clone());
                catalog_models.set(provider.models.clone());
                chat_status.set(String::new());
                if let Some(first) = provider.models.first() {
                    chat_model.set(first.reference.clone());
                    chat_context.set(first.context_window.to_string());
                    chat_output.set(first.output_limit.to_string());
                }
                let key = chat_secret.get_untracked();
                do_live_fetch(provider.base_url.clone(), key, false);
            } else {
                catalog_models.set(Vec::new());
                chat_status.set("Choose a provider and model from the catalog".into());
            }
        }
    };
    let on_model_select = move |event: leptos::ev::Event| {
        let model_reference = event_target_value(&event);
        chat_model.set(model_reference.clone());
        if let Some(model) = catalog_models
            .get_untracked()
            .into_iter()
            .find(|entry| entry.reference == model_reference)
        {
            chat_context.set(model.context_window.to_string());
            chat_output.set(model.output_limit.to_string());
        }
    };
    let on_base_url_input = {
        let do_live_fetch = do_live_fetch.clone();
        move |event: leptos::ev::Event| {
            let value = event_target_value(&event);
            chat_base_url.set(value.clone());
            let next_gen = live_generation.get_untracked().wrapping_add(1);
            live_generation.set(next_gen);
            let key = chat_secret.get_untracked();
            let do_live_fetch = do_live_fetch.clone();
            spawn_local(async move {
                wait_for_poll(520).await;
                if live_generation.get_untracked() != next_gen {
                    return;
                }
                let trimmed = value.trim().to_owned();
                if trimmed.len() < 8 {
                    return;
                }
                do_live_fetch(trimmed, key, false);
            });
        }
    };
    let on_secret_input = {
        let do_live_fetch = do_live_fetch.clone();
        move |event: leptos::ev::Event| {
            let value = event_target_value(&event);
            chat_secret.set(value.clone());
            let base = chat_base_url.get_untracked();
            if base.trim().is_empty() || value.trim().is_empty() {
                return;
            }
            let next_gen = live_generation.get_untracked().wrapping_add(1);
            live_generation.set(next_gen);
            let do_live_fetch = do_live_fetch.clone();
            spawn_local(async move {
                wait_for_poll(700).await;
                if live_generation.get_untracked() != next_gen {
                    return;
                }
                do_live_fetch(base, value, false);
            });
        }
    };
    let detect = move |_| {
        if !detecting.get_untracked() {
            load_auto_detect(detected, detecting, status);
        }
    };
    let delete_provider = {
        let chat_status = chat_status;
        let chat_configurations = chat_configurations;
        let status = status;
        let configurations = configurations;
        let lifecycle = Arc::clone(&lifecycle);
        std::sync::Arc::new(move |id: String| {
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                let response = Request::delete(&format!("/api/v1/providers/{id}"))
                    .send()
                    .await;
                match response {
                    Ok(resp) if resp.status() == 204 => {
                        chat_status.set("Provider deleted.".into());
                        status.set("Provider deleted.".into());
                        load_chat_models(chat_configurations, chat_status, Arc::clone(&lifecycle));
                        load_configurations(configurations, status);
                    }
                    Ok(resp) if resp.status() == 409 => {
                        let msg = api_error(&resp).await;
                        chat_status.set(format!("Cannot delete provider: {msg}"));
                        status.set(format!("Cannot delete provider: {msg}"));
                    }
                    Ok(resp) if resp.status() == 403 => {
                        chat_status.set("Owner/admin role required to delete providers.".into());
                        status.set("Owner/admin role required to delete providers.".into());
                    }
                    Ok(resp) => {
                        let msg = api_error(&resp).await;
                        if msg.to_lowercase().contains("forbidden")
                            || msg.to_lowercase().contains("admin")
                            || msg.to_lowercase().contains("owner")
                        {
                            chat_status
                                .set("Owner/admin role required to delete providers.".into());
                            status.set("Owner/admin role required to delete providers.".into());
                        } else {
                            chat_status.set(format!("Delete failed: {msg}"));
                            status.set(format!("Delete failed: {msg}"));
                        }
                    }
                    Err(_) => {
                        chat_status.set("Provider service did not answer.".into());
                        status.set("Provider service did not answer.".into());
                    }
                }
            });
        })
    };
    let delete_model = {
        let chat_status = chat_status;
        let chat_configurations = chat_configurations;
        let lifecycle = Arc::clone(&lifecycle);
        std::sync::Arc::new(move |id: String| {
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                let response = Request::delete(&format!("/api/v1/models/{id}"))
                    .send()
                    .await;
                match response {
                    Ok(resp) if resp.status() == 204 => {
                        chat_status.set("Model deleted.".into());
                        load_chat_models(chat_configurations, chat_status, Arc::clone(&lifecycle));
                    }
                    Ok(resp) if resp.status() == 409 => {
                        let msg = api_error(&resp).await;
                        chat_status.set(format!("Cannot delete model: {msg}"));
                    }
                    Ok(resp) if resp.status() == 403 => {
                        chat_status.set("Owner/admin role required to delete models.".into());
                    }
                    Ok(resp) => {
                        let msg = api_error(&resp).await;
                        if msg.to_lowercase().contains("forbidden")
                            || msg.to_lowercase().contains("admin")
                            || msg.to_lowercase().contains("owner")
                        {
                            chat_status.set("Owner/admin role required to delete models.".into());
                        } else {
                            chat_status.set(format!("Delete failed: {msg}"));
                        }
                    }
                    Err(_) => chat_status.set("Model service did not answer.".into()),
                }
            });
        })
    };
    let delete_embedding_config = {
        let status = status;
        let configurations = configurations;
        std::sync::Arc::new(move |id: String| {
            spawn_local(async move {
                let response = Request::delete(&format!("/api/v1/embeddings/configurations/{id}"))
                    .send()
                    .await;
                match response {
                    Ok(resp) if resp.status() == 204 => {
                        status.set("Embedding configuration deleted.".into());
                        load_configurations(configurations, status);
                    }
                    Ok(resp) if resp.status() == 409 => {
                        let msg = api_error(&resp).await;
                        status.set(format!("Cannot delete embedding configuration: {msg}"));
                    }
                    Ok(resp) if resp.status() == 403 => {
                        status.set(
                            "Owner/admin role required to delete embedding configurations.".into(),
                        );
                    }
                    Ok(resp) => {
                        let msg = api_error(&resp).await;
                        if msg.to_lowercase().contains("forbidden")
                            || msg.to_lowercase().contains("admin")
                            || msg.to_lowercase().contains("owner")
                        {
                            status.set(
                                "Owner/admin role required to delete embedding configurations."
                                    .into(),
                            );
                        } else {
                            status.set(format!("Delete failed: {msg}"));
                        }
                    }
                    Err(_) => status.set("Embedding service did not answer.".into()),
                }
            });
        })
    };
    let create = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let endpoint = base_url.get_untracked();
        let model_ref = model.get_untracked();
        let Ok(vector_dimensions) = dimensions.get_untracked().parse::<i32>() else {
            status.set("Dimensions must be a number.".into());
            return;
        };
        status.set("Validating and activating provider...".into());
        spawn_local(async move {
            let request = Request::post("/api/v1/embeddings/configurations").json(
                &CreateEmbeddingConfiguration {
                    display_name: "Local Ollama embeddings",
                    provider_type: "ollama",
                    base_url: endpoint.trim(),
                    secret_reference: None,
                    model_reference: model_ref.trim(),
                    dimensions: vector_dimensions,
                    activate: true,
                },
            );
            match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => load_configurations(configurations, status),
                    Ok(response) => {
                        status.set(format!("Provider rejected: HTTP {}", response.status()))
                    }
                    Err(_) => status.set("Provider service did not answer.".into()),
                },
                Err(_) => status.set("Provider request could not be encoded.".into()),
            }
        });
    };
    let create_chat_lifecycle = Arc::clone(&lifecycle);
    let create_chat = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        if catalog_models.get_untracked().is_empty() {
            chat_status.set("Choose a provider and model from the catalog".into());
            return;
        }
        let provider = chat_provider.get_untracked();
        let endpoint = chat_base_url.get_untracked();
        let model_reference = chat_model.get_untracked();
        if model_reference.trim().is_empty() {
            chat_status.set("Choose a model from the catalog".into());
            return;
        }
        let secret = chat_secret.get_untracked();
        let fallback_values = chat_fallbacks
            .get_untracked()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let (Ok(context_window), Ok(output_limit)) = (
            chat_context.get_untracked().parse::<i32>(),
            chat_output.get_untracked().parse::<i32>(),
        ) else {
            chat_status.set("Context and output limits must be numbers.".into());
            return;
        };
        chat_status.set("Validating and activating chat provider...".into());
        let lifecycle = Arc::clone(&create_chat_lifecycle);
        let base_url = endpoint.trim().to_owned();
        spawn_local(async move {
            if !lifecycle_is_active(&lifecycle) {
                return;
            }
            // The user may paste a raw API key (recommended) or an existing
            // vault secret id (secret_...). A raw key is stored in the vault
            // as a provider_credential secret scoped to the base-URL host,
            // then its reference is passed to the model route.
            let trimmed = secret.trim().to_owned();
            let secret_reference = if trimmed.is_empty() {
                None
            } else if trimmed.starts_with("secret_") {
                Some(trimmed.clone())
            } else {
                let host = url::Url::parse(&base_url)
                    .ok()
                    .and_then(|parsed| parsed.host_str().map(str::to_owned))
                    .unwrap_or_default();
                let store = Request::post("/api/v1/vault/secrets").json(&StoreSecretPayload {
                    purpose: "provider_credential",
                    allowed_hosts: vec![host.clone()],
                    value: trimmed.clone(),
                });
                match store {
                    Ok(store) => match store.send().await {
                        Ok(response) if response.ok() => {
                            match response.json::<SecretMeta>().await {
                                Ok(meta) => {
                                    chat_status.set(format!(
                                    "API key stored in vault (host {host}); creating chat route..."
                                ));
                                    Some(meta.id)
                                }
                                Err(_) => {
                                    chat_status.set(
                                        "Vault stored the key but the response was not valid."
                                            .into(),
                                    );
                                    return;
                                }
                            }
                        }
                        Ok(response) => {
                            chat_status.set(format!(
                                "Could not store the API key in the vault: {}",
                                api_error(&response).await
                            ));
                            return;
                        }
                        Err(_) => {
                            chat_status.set("Vault service did not answer.".into());
                            return;
                        }
                    },
                    Err(_) => {
                        chat_status.set("Vault request could not be encoded.".into());
                        return;
                    }
                }
            };
            let fallback_model_ids = fallback_values.iter().map(String::as_str).collect();
            let request =
                Request::post("/api/v1/models/chat").json(&CreateChatModelConfiguration {
                    display_name: model_reference.trim(),
                    provider_type: provider.trim(),
                    base_url: &base_url,
                    secret_reference: secret_reference.as_deref(),
                    model_reference: model_reference.trim(),
                    context_window,
                    output_limit,
                    priority: 0,
                    activate: true,
                    fallback_model_ids,
                });
            match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        chat_status.set("Chat route created and activated.".into());
                        load_chat_models(chat_configurations, chat_status, Arc::clone(&lifecycle));
                    }
                    Ok(response) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        chat_status.set(format!(
                            "Chat provider rejected: HTTP {}",
                            response.status()
                        ));
                    }
                    Err(_) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        chat_status.set("Chat provider service did not answer.".into());
                    }
                },
                Err(_) => {
                    if lifecycle_is_active(&lifecycle) {
                        chat_status.set("Chat provider request could not be encoded.".into());
                    }
                }
            }
        });
    };
    view! {
        <div class="page-heading"><div><p class="utility">"MODELS / ROUTES"</p><h1>"Provider registry"</h1></div>
            <button class="secondary" disabled=move || detecting.get() on:click=detect>
                {move || if detecting.get() { "Scanning..." } else { "Detect providers" }}
            </button>
        </div>
        <section class="model-section">
            <div class="section-heading"><div><p class="utility">"DETECTED / AUTO"</p><h2>"Available providers"</h2></div></div>
            <p class="form-note">{move || status.get()}</p>
            <div class="index-table">
                <div class="index-row header"><span>"PROVIDER"</span><span>"AVAILABLE"</span><span>"MODELS"</span></div>
                {move || detected.get().into_iter().map(|provider| view! {
                    <div class="index-row">
                        <span class="spine-cell">{provider.provider_type.to_uppercase()}</span>
                        <span>{if provider.available { "YES" } else { "NO" }}</span>
                        <span>{if provider.models.is_empty() { "—".into() } else { provider.models.join(", ") }}</span>
                    </div>
                }).collect_view()}
            </div>
        </section>
        <section class="model-section">
            <div class="section-heading"><div><p class="utility">"CHAT / STREAMING"</p><h2>"Conversation models"</h2></div><span>{move || format!("{} ROUTES", chat_configurations.get().len())}</span></div>
            {move || if chat_configurations.get().is_empty() {
                view! {
                    <div class="provider-empty-cta" role="status" aria-live="polite">
                        <div class="provider-empty-spine"><span>"0"</span></div>
                        <div>
                            <h3>"No chat route yet — get to first chat in 30 seconds"</h3>
                            <p>"Pick a preset, paste your API key, and Add + activate. Local Ollama needs no key. The active route handles all Chat requests."</p>
                            <div class="preset-row">
                                <button type="button" class="preset-chip openai" on:click=move |_| apply_preset("openai")>"OpenAI · gpt-4o"</button>
                                <button type="button" class="preset-chip anthropic" on:click=move |_| apply_preset("anthropic")>"Anthropic · claude-3"</button>
                                <button type="button" class="preset-chip ollama" on:click=move |_| apply_preset("ollama")>"Ollama · local"</button>
                                <button type="button" class="preset-chip ghost" on:click=move |_| apply_preset("custom")>"Custom"</button>
                            </div>
                        </div>
                    </div>
                }.into_any()
            } else { view! { <span></span> }.into_any() }}
            <div class="preset-row" aria-label="Provider presets">
                <span class="utility">"PRESETS"</span>
                <button type="button" class="preset-chip" on:click=move |_| apply_preset("openai")>"OpenAI"</button>
                <button type="button" class="preset-chip" on:click=move |_| apply_preset("anthropic")>"Anthropic"</button>
                <button type="button" class="preset-chip" on:click=move |_| apply_preset("ollama")>"Ollama"</button>
                <button type="button" class="preset-chip ghost" on:click=move |_| apply_preset("custom")>"Custom"</button>
            </div>
            <form class="model-form" on:submit=create_chat aria-label="Create chat model route">
                <label>"Provider type"
                    <select required prop:value=move || chat_provider.get() on:change=on_provider_select aria-label="Provider type">
                        <option value="">"Choose provider"</option>
                        {move || catalog.get().into_iter().map(|entry| {
                            let provider_type = entry.provider_type.clone();
                            let display = entry.display_name.clone();
                            view! { <option value=provider_type>{display}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <label>"Base URL"
                    <input required prop:value=move || chat_base_url.get() on:input=on_base_url_input aria-label="Base URL" />
                </label>
                {move || if live_pending.get() { view! { <span class="live-hint">"live"</span> }.into_any() } else { view! { <span></span> }.into_any() }}
                <label>"Model reference"
                    <select required prop:value=move || chat_model.get() on:change=on_model_select aria-label="Model reference">
                        <option value="">"Choose model"</option>
                        {move || catalog_models.get().into_iter().map(|entry| {
                            let display = entry.reference.clone();
                            let value = display.clone();
                            view! { <option value=value>{display}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <label>"Provider API key (optional — stored in the vault)"
                    <input maxlength="2048" autocomplete="off" placeholder="Paste API key or secret_… — vault-scoped to host" prop:value=move || chat_secret.get() on:input=on_secret_input aria-label="Provider API key" /></label>
                <label>"Context window"<input required inputmode="numeric" prop:value=move || chat_context.get() on:input=move |event| chat_context.set(event_target_value(&event)) aria-label="Context window" /></label>
                <label>"Output limit"<input required inputmode="numeric" prop:value=move || chat_output.get() on:input=move |event| chat_output.set(event_target_value(&event)) aria-label="Output limit" /></label>
                <label class="fallback-field">"Fallback model IDs"<input placeholder="Comma-separated, in failover order" prop:value=move || chat_fallbacks.get() on:input=move |event| chat_fallbacks.set(event_target_value(&event)) aria-label="Fallback model IDs" /></label>
                <div class="model-form-actions" style="display:flex;gap:8px;align-items:center">
                    <button type="button" class="secondary" disabled=move || test_pending.get() on:click=do_test aria-live="polite">
                        {move || if test_pending.get() { "Testing…" } else { "Test" }}
                    </button>
                    <button class="primary" type="submit">"Add + activate"</button>
                    <span class="form-note" aria-live="polite">{move || {
                        if test_pending.get() { "◌ Testing…".into() } else { test_message.get() }
                    }}</span>
                    <span class={move || match test_ok.get() { Some(true) => "test-success", Some(false) => "test-error", None => "form-note" }}>{move || if test_ok.get().is_some() { if test_ok.get().unwrap() { "✓ ".to_owned()+&test_message.get() } else { "✗ ".to_owned()+&test_message.get() } } else { "".into() }}</span>
                </div>
            </form>
            <p class="form-note" aria-live="polite">{move || chat_status.get()}</p>
            <div class="index-table model-index" role="table" aria-label="Chat model routes">
                <div class="index-row header"><span>"STATE"</span><span>"MODEL"</span><span>"LIMITS"</span><span>"ACTIONS"</span></div>
                {move || chat_configurations.get().into_iter().map(|configuration| {
                    let model_id = configuration.id.clone();
                    let provider_id = configuration.provider_id.clone();
                    let model_label = format!("{} / {}", configuration.display_name, configuration.model_reference);
                    let provider_label = format!("{} ({})", configuration.provider_type, &provider_id[..8.min(provider_id.len())]);
                    let activate_lifecycle = Arc::clone(&lifecycle);
                    let delete_target_model = delete_target;
                    let delete_target_provider = delete_target;
                    view! {
                        <div class="index-row model-route-row">
                            <span class="spine-cell">{if configuration.active { "ACTIVE" } else { "READY" }}</span>
                            <div style="display:grid;gap:4px;min-width:0">
                                <strong style="overflow-wrap:anywhere">{model_label.clone()}</strong>
                                <span class="utility" style="font-size:10px">{provider_label.clone()}</span>
                            </div>
                            <span>{format!("{} · {}K / {}", configuration.provider_type, configuration.context_window / 1000, configuration.output_limit)}</span>
                            <div style="display:flex;gap:6px;align-items:center;flex-wrap:wrap">
                                <button class="text-button" disabled=configuration.active on:click=move |_| activate_chat_model(model_id.clone(), chat_configurations, chat_status, Arc::clone(&activate_lifecycle))>
                                    {if configuration.active { format!("{} FALLBACKS", configuration.fallback_model_ids.len()) } else { "Activate".into() }}
                                </button>
                                <button class="icon-button danger" type="button" title="Delete model" aria-label=format!("Delete model {}", configuration.model_reference) on:click=move |_| delete_target_model.set(Some(DeleteTarget::Model { id: configuration.id.clone(), label: format!("{} / {}", configuration.display_name, configuration.model_reference) }))>"🗑"</button>
                                <button class="icon-button danger" type="button" title="Delete provider (and its models)" aria-label=format!("Delete provider {}", configuration.provider_type) on:click=move |_| delete_target_provider.set(Some(DeleteTarget::Provider { id: configuration.provider_id.clone(), label: configuration.provider_type.clone() }))>"⌫"</button>
                            </div>
                        </div>
                    }
                }).collect_view()}
            </div>
            {move || {
                let dp = std::sync::Arc::clone(&delete_provider);
                let dm = std::sync::Arc::clone(&delete_model);
                let de = std::sync::Arc::clone(&delete_embedding_config);
                delete_target.get().clone().map(|target| {
                let (title, body, is_provider) = match target.clone() {
                    DeleteTarget::Provider { id, label } => (format!("Delete provider {label}?"), format!("This will permanently delete the provider and ALL its models. This cannot be undone. Provider id: {}", &id[..8.min(id.len())]), true),
                    DeleteTarget::Model { id, label } => (format!("Delete model {label}?"), format!("This will permanently delete the model. If it is the active route, the delete will be refused (409). Id: {}", &id[..8.min(id.len())]), false),
                    DeleteTarget::EmbeddingConfig { id, label } => (format!("Delete embedding config {label}?"), format!("This will permanently delete the embedding configuration. Active configs or those with jobs will be refused (409). Id: {}", &id[..8.min(id.len())]), false),
                    _ => (String::new(), String::new(), false),
                };
                let confirm_target = target.clone();
                let dp_inner = std::sync::Arc::clone(&dp);
                let dm_inner = std::sync::Arc::clone(&dm);
                let de_inner = std::sync::Arc::clone(&de);
                view! {
                    <div class="modal-backdrop" role="dialog" aria-modal="true">
                        <div class="modal delete-confirm">
                            <div class="modal-head"><h3>{title.clone()}</h3><button class="text-button" on:click=move |_| delete_target.set(None)>"✕"</button></div>
                            <p class="form-note">{body.clone()}</p>
                            <p class="error-note" style="font-size:13px">{if is_provider { "All models under this provider will be removed." } else { "The record will be removed." }}</p>
                            <div style="display:flex;gap:10px;justify-content:flex-end;margin-top:12px">
                                <button class="secondary" on:click=move |_| delete_target.set(None)>"Cancel"</button>
                                <button class="primary danger" on:click=move |_| {
                                    match confirm_target.clone() {
                                        DeleteTarget::Provider { id, .. } => dp_inner(id),
                                        DeleteTarget::Model { id, .. } => dm_inner(id),
                                        DeleteTarget::EmbeddingConfig { id, .. } => de_inner(id),
                                        _ => {}
                                    }
                                    delete_target.set(None);
                                }>"Delete"</button>
                            </div>
                        </div>
                    </div>
                }
            })}}
        </section>
        <section class="model-section">
        <div class="section-heading"><div><p class="utility">"LIBRARY / RETRIEVAL"</p><h2>"Embedding models"</h2></div></div>
        <form class="provider-form" on:submit=create>
            <label>"Ollama base URL"<input required prop:value=move || base_url.get() on:input=move |event| base_url.set(event_target_value(&event)) /></label>
            <label>"Model reference"<input required prop:value=move || model.get() on:input=move |event| model.set(event_target_value(&event)) /></label>
            <label>"Dimensions"<input required inputmode="numeric" prop:value=move || dimensions.get() on:input=move |event| dimensions.set(event_target_value(&event)) /></label>
            <button class="primary" type="submit">"Add + activate"</button>
        </form>
        <p class="form-note">{move || status.get()}</p>
        <div class="index-table">
            <div class="index-row header"><span>"STATE"</span><span>"MODEL"</span><span>"PROVIDER"</span><span>"ACTIONS"</span></div>
            {move || configurations.get().into_iter().map(|configuration| {
                let cfg_id = configuration.id.clone();
                let cfg_label = format!("{} / {}", configuration.display_name, configuration.model_reference);
                view! {
                    <div class="index-row">
                        <span class="spine-cell">{if configuration.active { "ACTIVE" } else { "READY" }}</span>
                        <strong>{format!("{} / {}", configuration.display_name, configuration.model_reference)}</strong>
                        <span>{format!("{} · {}", configuration.provider_type, &configuration.id[..8.min(configuration.id.len())])}</span>
                        <div style="display:flex;gap:6px">
                            <button class="icon-button danger" type="button" title="Delete embedding configuration" aria-label=format!("Delete embedding configuration {}", cfg_label.clone()) on:click=move |_| delete_target.set(Some(DeleteTarget::EmbeddingConfig { id: cfg_id.clone(), label: cfg_label.clone() }))>"🗑"</button>
                        </div>
                    </div>
                }
            }).collect_view()}
        </div>
        </section>
        <section class="model-section">
            <div class="section-heading"><div><p class="utility">"USAGE / COST"</p><h2>"Model spend"</h2></div>
                <select aria-label="Usage window" prop:value=move || usage_window.get() on:change=move |event| {
                    let value = event_target_value(&event);
                    usage_window.set(value.clone());
                    let usage_lifecycle = Arc::clone(&usage_lifecycle);
                    spawn_local(async move {
                        let response = Request::get(&format!("/api/v1/usage/summary?window={value}"))
                            .send()
                            .await;
                        if !lifecycle_is_active(&usage_lifecycle) {
                            return;
                        }
                        match response {
                            Ok(response) if response.ok() => {
                                match response.json::<UsageSummary>().await {
                                    Ok(summary) => {
                                        usage.set(Some(summary));
                                        usage_status.set(String::new());
                                    }
                                    Err(_) => usage_status.set("Could not decode usage summary".into()),
                                }
                            }
                            Ok(response) => usage_status.set(format!("Usage endpoint rejected: HTTP {}", response.status())),
                            Err(_) => usage_status.set("Usage endpoint did not answer.".into()),
                        }
                    });
                }>
                    <option value="7d">"7 days"</option>
                    <option value="30d">"30 days"</option>
                    <option value="all">"All time"</option>
                </select>
            </div>
            <p class="form-note">{move || usage_status.get()}</p>
            {move || if let Some(summary) = usage.get() {
                view! {
                    <div class="usage-stats">
                        <div class="usage-stat"><span class="utility">"TOTAL SPEND"</span><strong>{format!("${:.2}", summary.total_spend)}</strong></div>
                        <div class="usage-stat"><span class="utility">"RUNS"</span><strong>{summary.total_runs}</strong></div>
                        <div class="usage-stat"><span class="utility">"INPUT"</span><strong>{format!("{}M", summary.total_input_tokens / 1_000_000)}</strong></div>
                        <div class="usage-stat"><span class="utility">"OUTPUT"</span><strong>{format!("{}M", summary.total_output_tokens / 1_000_000)}</strong></div>
                        <div class="usage-stat"><span class="utility">"UNPRICED"</span><strong>{summary.unpriced_runs}</strong></div>
                    </div>
                    <div class="usage-charts">
                        <div class="chart-card">
                            <p class="utility">"SPEND BY MODEL"</p>
                            <div class="bar-chart">
                                {summary.per_model.iter().map(|row| {
                                    let max = summary.per_model.first().map_or(0.01, |top| top.spend.max(0.01));
                                    let width = (row.spend / max * 100.0).max(1.0);
                                    view! {
                                        <div class="bar-row">
                                            <span class="bar-label">{format!("{} / {}", row.provider, row.model)}</span>
                                            <div class="bar-track"><div class="bar-fill" style=format!("width:{width:.0}%")></div></div>
                                            <span class="bar-value">{format!("${:.2}", row.spend)}</span>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        </div>
                        <div class="chart-card">
                            <p class="utility">"SPEND BY PROVIDER"</p>
                            <div class="bar-chart">
                                {summary.per_provider.iter().map(|row| {
                                    let max = summary.per_provider.first().map_or(0.01, |top| top.spend.max(0.01));
                                    let width = (row.spend / max * 100.0).max(1.0);
                                    view! {
                                        <div class="bar-row">
                                            <span class="bar-label">{row.provider.to_uppercase()}</span>
                                            <div class="bar-track"><div class="bar-fill" style=format!("width:{width:.0}%")></div></div>
                                            <span class="bar-value">{format!("${:.2}", row.spend)}</span>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        </div>
                    </div>
                    <div class="chart-card">
                        <p class="utility">"SPEND BY DAY"</p>
                        <div class="sparkline">
                            {summary.per_day.iter().map(|row| {
                                let max = summary.per_day.iter().map(|d| d.spend).fold(0.0, f64::max).max(0.01);
                                let height = (row.spend / max * 64.0).max(2.0);
                                view! {
                                    <div class="day-bar" title=format!("{} ${:.2}", row.day, row.spend) style=format!("height:{height:.0}px")></div>
                                }
                            }).collect_view()}
                        </div>
                    </div>
                }.into_any()
            } else {
                view! { <div class="empty-small">"No usage recorded yet. Spend appears after model runs complete."</div> }.into_any()
            }}
        </section>
    }
}

fn load_usage(
    usage: &RwSignal<Option<UsageSummary>>,
    status: &RwSignal<String>,
    window: &RwSignal<String>,
    lifecycle: Arc<AtomicBool>,
) {
    let usage = *usage;
    let status = *status;
    let window = *window;
    spawn_local(async move {
        let response = Request::get(&format!(
            "/api/v1/usage/summary?window={}",
            window.get_untracked()
        ))
        .send()
        .await;
        if !lifecycle_is_active(&lifecycle) {
            return;
        }
        match response {
            Ok(response) if response.ok() => {
                if let Ok(summary) = response.json::<UsageSummary>().await {
                    usage.set(Some(summary));
                } else {
                    status.set("Could not decode usage summary".into());
                }
            }
            Ok(response) => status.set(format!(
                "Usage endpoint rejected: HTTP {}",
                response.status()
            )),
            Err(_) => status.set("Usage endpoint did not answer.".into()),
        }
    });
}

#[component]
fn DiagnosticsPage() -> impl IntoView {
    let jobs = RwSignal::new(Vec::<EmbeddingJob>::new());
    let status = RwSignal::new(String::new());
    load_jobs(jobs, status);
    view! {
        <div class="page-heading"><div><p class="utility">"DIAGNOSTICS / INDEXING"</p><h1>"Embedding queue"</h1></div>
            <button class="secondary" on:click=move |_| load_jobs(jobs, status)>"Refresh"</button></div>
        <p class="form-note">{move || status.get()}</p>
        <div class="diagnostic-grid">
            {move || {
                let current = jobs.get();
                let active = current.iter().filter(|job| matches!(job.status.as_str(), "queued" | "retry" | "running")).count();
                let failed = current.iter().filter(|job| job.status == "failed").count();
                view! { <><div><span>"ACTIVE"</span><strong>{active}</strong></div><div><span>"FAILED"</span><strong>{failed}</strong></div><div><span>"VISIBLE JOBS"</span><strong>{current.len()}</strong></div></> }
            }}
        </div>
        <div class="index-table">
            <div class="index-row header"><span>"ID"</span><span>"STATE"</span><span>"ERROR"</span><span>"LEASED"</span></div>
            {move || jobs.get().into_iter().map(|job| view! { <div class="index-row">
                <span class="spine-cell">{job.id.chars().take(8).collect::<String>()}</span><strong>{job.status}</strong>
                <span>{job.last_error_code.unwrap_or_else(|| "NONE".into())}</span><span>"DURABLE"</span>
            </div> }).collect_view()}
        </div>
    }
}

#[component]
fn AutobiographyPage() -> impl IntoView {
    let body = RwSignal::new(String::new());
    let revision = RwSignal::new(0_i64);
    let policy = RwSignal::new(String::new());
    let updated_at = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    load_autobiography(body, revision, policy, updated_at, status);
    view! {
        <div class="page-heading">
            <div><p class="utility">"PROFILE / AUTOBIOGRAPHY"</p><h1>"Autobiography"</h1></div>
            <span class="utility">{move || format!("REV {}", revision.get())}</span>
        </div>
        <p class="form-note">{move || status.get()}</p>
        <div class="index-table">
            <div class="index-row"><span class="spine-cell">"REVISION"</span><span>{move || format!("v{}", revision.get())}</span></div>
            <div class="index-row"><span class="spine-cell">"POLICY"</span><span>{move || policy.get().to_uppercase()}</span></div>
            <div class="index-row"><span class="spine-cell">"UPDATED"</span><span>{move || updated_at.get()}</span></div>
        </div>
        <pre>{move || body.get()}</pre>
    }
}
// ---------------------------------------------------------------------------
// M24b: UI packages page — specimen-wall cards, state badges, activate/rollback/delete
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct UiListItem {
    id: String,
    name: String,
    version: String,
    ui_kind: String,
    state: String,
    trust: String,
    source_type: String,
    created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct UiAssetRow {
    file_path: String,
    content_type: String,
    sha256_hash: String,
    file_size: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct UiDetail {
    id: String,
    profile_id: String,
    name: String,
    version: String,
    ui_kind: String,
    description: String,
    source_type: String,
    source_uri: String,
    state: String,
    trust: String,
    manifest: serde_json::Value,
    artifact_digest: Option<String>,
    install_path: Option<String>,
    entry_point: Option<String>,
    assets: Vec<UiAssetRow>,
    capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct UiPreviewResponse {
    digest: String,
    manifest: serde_json::Value,
    ui_kind: String,
    capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct UiInstallResponse {
    id: String,
    state: String,
    name: String,
    version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct UiActivateResponse {
    id: String,
    state: String,
    previous_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct UiPreviewRequest<'a> {
    source_type: &'a str,
    source_uri: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct UiInstallRequest<'a> {
    source_type: &'a str,
    source_uri: &'a str,
    expected_digest: &'a str,
    approve: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a str>,
}

fn ui_state_class(state: &str) -> &'static str {
    match state {
        "active" => "active",
        "candidate" | "validated" => "candidate",
        "previous" => "previous",
        "staged" | "discovered" => "staged",
        "failed" | "rolled_back" => "failed",
        _ => "staged",
    }
}

fn ui_kind_class(kind: &str) -> &'static str {
    match kind {
        "THEME" => "theme",
        "FULL_UI" => "full",
        _ => "theme",
    }
}

#[component]
fn UiPackagesPage() -> impl IntoView {
    let lifecycle = Arc::new(AtomicBool::new(true));
    on_cleanup({
        let lifecycle = Arc::clone(&lifecycle);
        move || lifecycle.store(false, Ordering::Release)
    });
    let packages = RwSignal::new(Vec::<UiListItem>::new());
    let status = RwSignal::new(String::new());
    let error = RwSignal::new(Option::<String>::None);
    let detail = RwSignal::new(Option::<UiDetail>::None);
    let preview = RwSignal::new(Option::<UiPreviewResponse>::None);
    let digest = RwSignal::new(String::new());
    // install form
    let source_type = RwSignal::new("local_package".to_owned());
    let source_uri = RwSignal::new(String::new());
    let manifest_json = RwSignal::new(String::new());
    let version = RwSignal::new(String::new());
    let confirmed = RwSignal::new(false);
    let installing = RwSignal::new(false);
    let expanded = RwSignal::new(Option::<String>::None);
    let confirm_activate = RwSignal::new(Option::<String>::None);
    let delete_target_ui = RwSignal::new(None::<DeleteTarget>);

    let load = {
        let lifecycle = Arc::clone(&lifecycle);
        move || {
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                match Request::get("/api/v1/ui/packages").send().await {
                    Ok(resp) if resp.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        match resp.json::<Vec<UiListItem>>().await {
                            Ok(list) => {
                                packages.set(list);
                                status.set(String::new());
                                error.set(None);
                            }
                            Err(_) => error.set(Some("Could not decode package list.".into())),
                        }
                    }
                    Ok(resp) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some(format!("List failed: HTTP {}", resp.status())));
                    }
                    Err(_) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some("Package service did not answer.".into()));
                    }
                }
            });
        }
    };
    // theme overlay: inject active theme css if present
    {
        let lifecycle = Arc::clone(&lifecycle);
        spawn_local(async move {
            let resp = Request::get("/api/v1/ui/active-theme.css").send().await;
            if !lifecycle_is_active(&lifecycle) {
                return;
            }
            #[allow(clippy::collapsible_if)]
            if let Ok(r) = resp
                && r.ok()
                && let Ok(css) = r.text().await
                && !css.trim().is_empty()
                && !css.contains("no active theme")
            {
                if let Some(window) = web_sys::window()
                    && let Some(doc) = window.document()
                    && let Some(head) = doc.head()
                {
                    if let Some(existing) = doc.get_element_by_id("gobrowse-theme") {
                        existing.remove();
                    }
                    if let Ok(style) = doc.create_element("style") {
                        style.set_id("gobrowse-theme");
                        style.set_text_content(Some(&css));
                        let _ = head.append_child(&style);
                    }
                }
            }
        });
    }
    load();
    let do_preview = {
        let lifecycle = Arc::clone(&lifecycle);
        move |_| {
            let st = source_type.get_untracked();
            let uri = source_uri.get_untracked();
            let mj = manifest_json.get_untracked();
            let ver = version.get_untracked();
            let manifest_val: Option<serde_json::Value> = if mj.trim().is_empty() {
                None
            } else {
                serde_json::from_str(mj.trim()).ok()
            };
            let uri_or_inline = if !mj.trim().is_empty() && uri.trim().is_empty() {
                format!("inline:{}", mj.trim())
            } else {
                uri.clone()
            };
            if uri_or_inline.trim().is_empty() && manifest_val.is_none() {
                error.set(Some("Provide source URI or paste manifest JSON.".into()));
                return;
            }
            status.set("Resolving manifest…".into());
            error.set(None);
            preview.set(None);
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                let body = UiPreviewRequest {
                    source_type: st.trim(),
                    source_uri: uri_or_inline.trim(),
                    manifest: manifest_val,
                    version: if ver.trim().is_empty() {
                        None
                    } else {
                        Some(ver.trim())
                    },
                };
                let req = Request::post("/api/v1/ui/preview").json(&body);
                match req {
                    Ok(req) => match req.send().await {
                        Ok(resp) if resp.ok() => {
                            if !lifecycle_is_active(&lifecycle) {
                                return;
                            }
                            match resp.json::<UiPreviewResponse>().await {
                                Ok(p) => {
                                    digest.set(p.digest.clone());
                                    preview.set(Some(p));
                                    status.set(
                                        "Preview ready — review and approve to install.".into(),
                                    );
                                }
                                Err(_) => error.set(Some("Preview response invalid.".into())),
                            }
                        }
                        Ok(resp) => {
                            if !lifecycle_is_active(&lifecycle) {
                                return;
                            }
                            error.set(Some(format!(
                                "Preview rejected: {}",
                                api_error(&resp).await
                            )));
                            status.set(String::new());
                        }
                        Err(_) => {
                            if !lifecycle_is_active(&lifecycle) {
                                return;
                            }
                            error.set(Some("Preview service did not answer.".into()));
                        }
                    },
                    Err(_) => {
                        if lifecycle_is_active(&lifecycle) {
                            error.set(Some("Preview request could not be encoded.".into()));
                        }
                    }
                }
            });
        }
    };
    let do_install = {
        let lifecycle = Arc::clone(&lifecycle);
        move |_| {
            if !confirmed.get_untracked() {
                error.set(Some("Confirm approval before install.".into()));
                return;
            }
            let st = source_type.get_untracked();
            let uri = source_uri.get_untracked();
            let mj = manifest_json.get_untracked();
            let manifest_val: Option<serde_json::Value> = if mj.trim().is_empty() {
                None
            } else {
                serde_json::from_str(mj.trim()).ok()
            };
            let uri_or_inline = if !mj.trim().is_empty() && uri.trim().is_empty() {
                format!("inline:{}", mj.trim())
            } else {
                uri.clone()
            };
            let dig = digest.get_untracked();
            if dig.trim().is_empty() {
                error.set(Some("Preview first to get a digest.".into()));
                return;
            }
            installing.set(true);
            error.set(None);
            status.set("Installing…".into());
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                let body = UiInstallRequest {
                    source_type: st.trim(),
                    source_uri: uri_or_inline.trim(),
                    expected_digest: dig.trim(),
                    approve: true,
                    manifest: manifest_val,
                    workspace_id: None,
                };
                let req = Request::post("/api/v1/ui/install").json(&body);
                match req {
                    Ok(req) => match req.send().await {
                        Ok(resp) if resp.ok() => {
                            if !lifecycle_is_active(&lifecycle) {
                                installing.set(false);
                                return;
                            }
                            match resp.json::<UiInstallResponse>().await {
                                Ok(inst) => {
                                    status.set(format!(
                                        "Installed {} {} — {}",
                                        inst.name, inst.version, inst.state
                                    ));
                                    preview.set(None);
                                    digest.set(String::new());
                                    confirmed.set(false);
                                    installing.set(false);
                                    // reload list
                                    match Request::get("/api/v1/ui/packages").send().await {
                                        Ok(r) if r.ok() => {
                                            if let Ok(list) = r.json::<Vec<UiListItem>>().await {
                                                packages.set(list);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                Err(_) => {
                                    error.set(Some("Install response invalid.".into()));
                                    installing.set(false);
                                }
                            }
                        }
                        Ok(resp) => {
                            if !lifecycle_is_active(&lifecycle) {
                                installing.set(false);
                                return;
                            }
                            error.set(Some(format!(
                                "Install rejected: {}",
                                api_error(&resp).await
                            )));
                            status.set(String::new());
                            installing.set(false);
                        }
                        Err(_) => {
                            if !lifecycle_is_active(&lifecycle) {
                                installing.set(false);
                                return;
                            }
                            error.set(Some("Install service did not answer.".into()));
                            installing.set(false);
                        }
                    },
                    Err(_) => {
                        if lifecycle_is_active(&lifecycle) {
                            error.set(Some("Install request could not be encoded.".into()));
                        }
                        installing.set(false);
                    }
                }
            });
        }
    };
    let activate = std::sync::Arc::new({
        let lc = Arc::clone(&lifecycle);
        move |id: String| {
            let lifecycle = Arc::clone(&lc);
            spawn_local(async move {
                let resp = Request::post(&format!("/api/v1/ui/packages/{id}/activate"))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        status.set("Activated — reloading.".into());
                        error.set(None);
                        confirm_activate.set(None);
                        match Request::get("/api/v1/ui/packages").send().await {
                            Ok(rr) if rr.ok() => {
                                if let Ok(list) = rr.json::<Vec<UiListItem>>().await {
                                    packages.set(list);
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(r) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some(format!("Activate failed: {}", api_error(&r).await)));
                    }
                    Err(_) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some("Activate service did not answer.".into()));
                    }
                }
            });
        }
    });
    let rollback = {
        let lifecycle = Arc::clone(&lifecycle);
        move |_| {
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                let resp = Request::post("/api/v1/ui/rollback").send().await;
                match resp {
                    Ok(r) if r.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        status.set("Rolled back to previous.".into());
                        error.set(None);
                        match Request::get("/api/v1/ui/packages").send().await {
                            Ok(rr) if rr.ok() => {
                                if let Ok(list) = rr.json::<Vec<UiListItem>>().await {
                                    packages.set(list);
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(r) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some(format!("Rollback failed: {}", api_error(&r).await)));
                    }
                    Err(_) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some("Rollback service did not answer.".into()));
                    }
                }
            });
        }
    };
    let delete_pkg = std::sync::Arc::new({
        let lc = Arc::clone(&lifecycle);
        move |id: String| {
            let lifecycle = Arc::clone(&lc);
            spawn_local(async move {
                let resp = Request::delete(&format!("/api/v1/ui/packages/{id}"))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status() == 204 || r.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        status.set("Deleted.".into());
                        error.set(None);
                        match Request::get("/api/v1/ui/packages").send().await {
                            Ok(rr) if rr.ok() => {
                                if let Ok(list) = rr.json::<Vec<UiListItem>>().await {
                                    packages.set(list);
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(r) if r.status() == 409 => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        let msg = api_error(&r).await;
                        error.set(Some(format!("Cannot delete package: {msg}")));
                    }
                    Ok(r) if r.status() == 403 => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some(
                            "Owner/admin role required to delete UI packages.".into(),
                        ));
                    }
                    Ok(r) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        let msg = api_error(&r).await;
                        if msg.to_lowercase().contains("forbidden")
                            || msg.to_lowercase().contains("admin")
                            || msg.to_lowercase().contains("owner")
                        {
                            error.set(Some(
                                "Owner/admin role required to delete UI packages.".into(),
                            ));
                        } else {
                            error.set(Some(format!("Delete rejected: {msg}")));
                        }
                    }
                    Err(_) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some("Delete service did not answer.".into()));
                    }
                }
            });
        }
    });
    let open_detail = std::sync::Arc::new({
        let lc = Arc::clone(&lifecycle);
        move |id: String| {
            let lifecycle = Arc::clone(&lc);
            spawn_local(async move {
                let resp = Request::get(&format!("/api/v1/ui/packages/{id}"))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.ok() => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        if let Ok(d) = r.json::<UiDetail>().await {
                            detail.set(Some(d));
                        }
                    }
                    Ok(r) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some(format!("Detail failed: {}", api_error(&r).await)));
                    }
                    Err(_) => {
                        if !lifecycle_is_active(&lifecycle) {
                            return;
                        }
                        error.set(Some("Detail service did not answer.".into()));
                    }
                }
            });
        }
    });
    view! {
        <div class="page-heading">
            <div><p class="utility">"DISPLAY / UI PACKAGES"</p><h1>"Interface packages"</h1></div>
            <span class="utility">{move || format!("{} PACKAGES", packages.get().len())}</span>
        </div>
        <p class="form-note">"Install themes (CSS overrides) or full WASM frontends. Activation is profile-scoped; the recovery UI at /recovery is always available. CSP is computed server-side; no unsafe-inline."</p>
        <div class="ui-action-bar">
            <button class="secondary" on:click={
                let load = load.clone();
                move |_| load()
            }>"Refresh"</button>
            <button class="secondary" on:click=rollback>"Rollback to previous"</button>
            <a class="text-button" href="/recovery" target="_blank" rel="noreferrer">"Open recovery →"</a>
        </div>
        <p class="form-note">{move || status.get()}</p>
        {move || error.get().clone().map(|e| view! { <div class="inline-error" role="alert"><p>{e}</p></div> })}
        // Install form — specimen-wall top
        <section class="model-section ui-install">
            <div class="section-heading"><div><p class="utility">"INSTALL / LOCAL PACKAGE"</p><h2>"Add a package"</h2></div></div>
            <p class="form-note">"Paste a gobrowse-ui manifest JSON (kind=gobrowse-ui, ui_kind THEME or FULL_UI). Preview validates and returns a digest; installation requires approve + digest match."</p>
            <div class="ui-install-grid">
                <label>"Source type"
                    <select prop:value=move || source_type.get() on:change=move |ev| source_type.set(event_target_value(&ev))>
                        <option value="local_package">"local_package"</option>
                        <option value="github_release">"github_release"</option>
                        <option value="marketplace">"marketplace"</option>
                    </select>
                </label>
                <label>"Source URI (or leave empty when pasting manifest)"
                    <input placeholder="inline:{...} or https://github.com/org/repo or path" prop:value=move || source_uri.get() on:input=move |ev| source_uri.set(event_target_value(&ev)) />
                </label>
                <label>"Version override (optional)"
                    <input placeholder="1.0.0" prop:value=move || version.get() on:input=move |ev| version.set(event_target_value(&ev)) />
                </label>
                <label class="ui-manifest-field">"Manifest JSON (paste full JSON)"
                    <textarea rows="8" placeholder="Paste manifest JSON here" prop:value=move || manifest_json.get() on:input=move |ev| manifest_json.set(event_target_value(&ev))></textarea>
                </label>
            </div>
            <div class="ui-install-actions">
                <button class="secondary" type="button" on:click=do_preview>"Preview"</button>
                <label class="confirm-row" style="flex:1">
                    <input type="checkbox" prop:checked=move || confirmed.get() on:change=move |ev| confirmed.set(event_target_checked(&ev)) />
                    <span>"I approve installing this package (approve: true)."</span>
                </label>
                <button class="primary" type="button" disabled=move || installing.get() || !confirmed.get() on:click=do_install>
                    {move || if installing.get() { "Installing…" } else { "Approve & Install" }}
                </button>
            </div>
            {move || preview.get().clone().map(|p| view! {
                <div class="ui-preview">
                    <div class="manifest-head">
                        <div><h3>{format!("{} · {}", p.manifest.get("name").and_then(|v| v.as_str()).unwrap_or("?"), p.manifest.get("version").and_then(|v| v.as_str()).unwrap_or(&p.ui_kind))}</h3><p class="form-note">{"Kind: "}{p.ui_kind.clone()}{" · digest "}<span class="mono-break">{p.digest.clone()}</span></p></div>
                        <span class="state-badge candidate">{p.ui_kind.clone()}</span>
                    </div>
                    <div class="index-table">
                        <div class="index-row"><span class="spine-cell">"DIGEST"</span><span class="mono-break">{p.digest.clone()}</span></div>
                        <div class="index-row"><span class="spine-cell">"CAPABILITIES"</span><span>{if p.capabilities.is_empty() { "—".into() } else { p.capabilities.join(", ") }}</span></div>
                    </div>
                    <pre class="ui-manifest-pre">{serde_json::to_string_pretty(&p.manifest).unwrap_or_default()}</pre>
                </div>
            })}
        </section>
        // Package grid — specimen wall
        <section class="model-section">
            <div class="section-heading"><div><p class="utility">"INSTALLED"</p><h2>"Package wall"</h2></div><span class="utility">{move || format!("{} ITEMS", packages.get().len())}</span></div>
            {move || if packages.get().is_empty() {
                view! {
                    <div class="ui-empty">
                        <span class="index-spine">"∅"</span>
                        <div><h3>"No interface packages"</h3><p>"Install a THEME to recolor the shell or a FULL_UI to replace the WASM frontend. The built-in UI is always available via Rollback → built-in."</p></div>
                    </div>
                }.into_any()
            } else {
                view! {
                    <div class="ui-package-grid">
                        {packages.get().into_iter().map(|pkg| {
                            let open_detail = open_detail.clone();
                            let pid = pkg.id.clone();
                            let pid2 = pkg.id.clone();
                            let pid3 = pkg.id.clone();
                            let pid4 = pkg.id.clone();
                            let pid5 = pkg.id.clone();
                            let is_expanded = expanded.get() == Some(pkg.id.clone());
                            let state_cls = ui_state_class(&pkg.state);
                            let kind_cls = ui_kind_class(&pkg.ui_kind);
                            let is_active = pkg.state == "active";
                            let confirm_id = confirm_activate.get() == Some(pkg.id.clone());
                            view! {
                                <article class="ui-package-card" class:is-active=is_active>
                                    <div class="ui-card-head">
                                        <span class={format!("kind-badge {}", kind_cls)}>{pkg.ui_kind.clone()}</span>
                                        <strong>{pkg.name.clone()}</strong>
                                        <span class="mono-break" style="margin-left:auto;font-size:11px">{pkg.version.clone()}</span>
                                    </div>
                                    <div class="ui-card-badges">
                                        <span class={format!("state-badge {}", state_cls)}>{pkg.state.to_uppercase()}</span>
                                        <span class={format!("trust-badge {}", pkg.trust.to_lowercase())}>{pkg.trust.clone()}</span>
                                        <span class="cap-chip">{pkg.source_type.clone()}</span>
                                    </div>
                                    <p class="book-snippet">{format!("{} · {}", pkg.id[..8.min(pkg.id.len())].to_owned(), pkg.created_at.clone())}</p>
                                    <div class="ui-card-actions">
                                        <button class="text-button" on:click=move |_| {
                                            let id = pid.clone();
                                            if expanded.get() == Some(id.clone()) { expanded.set(None); } else { expanded.set(Some(id.clone())); open_detail(id); }
                                        }>{if is_expanded { "Hide" } else { "Detail" }}</button>
                                        <button class="primary" disabled=is_active on:click={
                                            let id = pid2.clone();
                                            let activate = activate.clone();
                                            move |_| {
                                                if pkg.ui_kind == "FULL_UI" {
                                                    confirm_activate.set(Some(id.clone()));
                                                } else {
                                                    activate(id.clone());
                                                }
                                            }
                                        }>{if is_active { "Active" } else { "Activate" }}</button>
                                        <button class="secondary danger" disabled=is_active title="Delete package" aria-label=format!("Delete package {}", pkg.name.clone()) on:click=move |_| delete_target_ui.set(Some(DeleteTarget::UiPackage { id: pid3.clone(), label: pkg.name.clone() }))>"Delete"</button>
                                    </div>
                                    {confirm_id.then(|| view! {
                                        <div class="ui-confirm">
                                            <p class="form-note">"FULL_UI replaces the entire frontend. Continue?"</p>
                                            <div style="display:flex;gap:8px">
                                                <button class="primary" on:click={
                                                    let activate = activate.clone();
                                                    let id = pid4.clone();
                                                    move |_| activate(id.clone())
                                                }>"Confirm activate"</button>
                                                <button class="secondary" on:click=move |_| confirm_activate.set(None)>"Cancel"</button>
                                            </div>
                                        </div>
                                    })}
                                    {is_expanded.then(|| view! {
                                        <div class="ui-detail">
                                            {detail.get().clone().map(|d| if d.id == pid5 {
                                                view! {
                                                    <div class="index-table">
                                                        <div class="index-row"><span class="spine-cell">"STATE"</span><span>{d.state.clone()}</span></div>
                                                        <div class="index-row"><span class="spine-cell">"TRUST"</span><span>{d.trust.clone()}</span></div>
                                                        <div class="index-row"><span class="spine-cell">"SOURCE"</span><span class="mono-break">{d.source_uri.clone()}</span></div>
                                                        <div class="index-row"><span class="spine-cell">"DIGEST"</span><span class="mono-break">{d.artifact_digest.clone().unwrap_or_else(|| "—".into())}</span></div>
                                                        <div class="index-row"><span class="spine-cell">"ENTRY"</span><span>{d.entry_point.clone().unwrap_or_else(|| "—".into())}</span></div>
                                                        <div class="index-row"><span class="spine-cell">"ASSETS"</span><span>{d.assets.len().to_string()}</span></div>
                                                        <div class="index-row"><span class="spine-cell">"CAPS"</span><span>{if d.capabilities.is_empty() { "—".into() } else { d.capabilities.join(", ") }}</span></div>
                                                    </div>
                                                    <pre class="ui-manifest-pre">{serde_json::to_string_pretty(&d.manifest).unwrap_or_default()}</pre>
                                                }.into_any()
                                            } else { view! { <p class="form-note">"Loading…"</p> }.into_any() })}
                                        </div>
                                    })}
                                </article>
                            }
                        }).collect_view()}
                    </div>
                }.into_any()
            }}
        </section>
        {move || {
            let dp = std::sync::Arc::clone(&delete_pkg);
            delete_target_ui.get().clone().map(|target| {
                let (title, body) = match target.clone() {
                    DeleteTarget::UiPackage { id, label } => (format!("Delete \"{label}\"?"), format!("This will permanently delete UI package {label}. Active/last packages will be refused (409). Id: {}", &id[..8.min(id.len())])),
                    _ => (String::new(), String::new()),
                };
                let confirm_target = target.clone();
                let dp_inner = std::sync::Arc::clone(&dp);
                view! {
                    <div class="modal-backdrop" role="dialog" aria-modal="true">
                        <div class="modal delete-confirm">
                            <div class="modal-head"><h3>{title.clone()}</h3><button class="text-button" on:click=move |_| delete_target_ui.set(None)>"✕"</button></div>
                            <p class="form-note">{body.clone()}</p>
                            <p class="error-note" style="font-size:13px">"This cannot be undone."</p>
                            <div style="display:flex;gap:10px;justify-content:flex-end;margin-top:12px">
                                <button class="secondary" on:click=move |_| delete_target_ui.set(None)>"Cancel"</button>
                                <button class="primary danger" on:click=move |_| {
                                    if let DeleteTarget::UiPackage { id, .. } = confirm_target.clone() { dp_inner(id); }
                                    delete_target_ui.set(None);
                                }>"Delete"</button>
                            </div>
                        </div>
                    </div>
                }
            })
        }}
        {move || detail.get().clone().map(|d| view! {
            <section class="model-section">
                <div class="section-heading"><div><p class="utility">"DETAIL"</p><h2>{d.name.clone()}</h2></div><button class="text-button" on:click=move |_| detail.set(None)>"Close"</button></div>
                <pre class="ui-manifest-pre">{serde_json::to_string_pretty(&d.manifest).unwrap_or_default()}</pre>
            </section>
        })}
    }
}
// ---------------------------------------------------------------------------
// M25a: Security — auth methods + passkey + sessions + step-up (Archive Spine)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct AuthMethod {
    id: String,
    method_type: String,
    label: Option<String>,
    is_primary: bool,
    last_used_at: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct MethodsResponse {
    methods: Vec<AuthMethod>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct SessionRow {
    hash: String,
    current: bool,
    created_at: String,
    last_seen_at: String,
    expires_at: String,
    last_step_up_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionsResponse {
    sessions: Vec<SessionRow>,
}

#[derive(Debug, Deserialize)]
struct WebauthnRegisterStart {
    ceremony_id: String,
    creation_options: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct WebauthnLoginStart {
    ceremony_id: String,
    request_options: serde_json::Value,
}

fn method_spine_class(method_type: &str) -> &'static str {
    match method_type {
        "webauthn" => "webauthn",
        "oidc" => "oidc",
        _ => "password",
    }
}

fn human_method_label(method: &AuthMethod) -> String {
    match method.method_type.as_str() {
        "webauthn" => "Passkey".into(),
        "password" => "Password".into(),
        "oidc" => "SSO".into(),
        other => other.into(),
    }
}

fn short_hash(hash: &str) -> String {
    if hash.len() <= 12 {
        hash.to_owned()
    } else {
        format!("{}…{}", &hash[..8], &hash[hash.len() - 4..])
    }
}

fn map_webauthn_error(value: &JsValue) -> String {
    let name = js_sys::Reflect::get(value, &JsValue::from_str("name"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let msg = js_sys::Reflect::get(value, &JsValue::from_str("message"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("{value:?}"));
    match name.as_str() {
        "NotAllowedError" => "The authenticator was cancelled or timed out.".into(),
        "InvalidStateError" => "This passkey is already registered.".into(),
        "NotSupportedError" => "This device does not support passkeys.".into(),
        "SecurityError" => "Passkeys require a secure context (HTTPS or localhost).".into(),
        _ if msg.contains("already") => "This credential is already registered.".into(),
        _ if msg.contains("verification failed") => msg,
        _ => msg,
    }
}

fn js_credential_to_json(value: JsValue) -> Result<serde_json::Value, String> {
    let s = js_sys::JSON::stringify(&value)
        .map_err(|_| "Invalid credential shape.".to_string())?
        .as_string()
        .ok_or_else(|| "Invalid credential string.".to_string())?;
    serde_json::from_str(&s).map_err(|_| "Invalid credential JSON.".to_string())
}

// WebAuthn JS bridge — CSP-safe (compiled into the WASM JS glue, not eval).
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function b64urlToBuf(b64url) {
  let s = b64url.replace(/-/g,'+').replace(/_/g,'/');
  while (s.length % 4) s += '=';
  const bin = atob(s);
  const buf = new Uint8Array(bin.length);
  for (let i=0;i<bin.length;i++) buf[i]=bin.charCodeAt(i);
  return buf.buffer;
}
export function bufToB64url(buf) {
  const bytes = new Uint8Array(buf);
  let s='';
  for (const b of bytes) s+=String.fromCharCode(b);
  return btoa(s).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'');
}
function transformCreate(o){
  const pk = o.publicKey ? o.publicKey : o;
  pk.challenge = b64urlToBuf(pk.challenge);
  pk.user.id = b64urlToBuf(pk.user.id);
  if(pk.excludeCredentials) for(const c of pk.excludeCredentials) c.id=b64urlToBuf(c.id);
  return {publicKey: pk};
}
function transformGet(o){
  const pk = o.publicKey ? o.publicKey : o;
  pk.challenge = b64urlToBuf(pk.challenge);
  if(pk.allowCredentials) for(const c of pk.allowCredentials) c.id=b64urlToBuf(c.id);
  return {publicKey: pk};
}
export async function createPasskey(opts){
  const c = await navigator.credentials.create(transformCreate(opts));
  return {
    id: c.id,
    rawId: bufToB64url(c.rawId),
    type: c.type,
    response: {
      clientDataJSON: bufToB64url(c.response.clientDataJSON),
      attestationObject: bufToB64url(c.response.attestationObject)
    }
  };
}
export async function getPasskey(opts){
  const c = await navigator.credentials.get(transformGet(opts));
  return {
    id: c.id,
    rawId: bufToB64url(c.rawId),
    type: c.type,
    response: {
      clientDataJSON: bufToB64url(c.response.clientDataJSON),
      authenticatorData: bufToB64url(c.response.authenticatorData),
      signature: bufToB64url(c.response.signature),
      userHandle: c.response.userHandle ? bufToB64url(c.response.userHandle) : null
    }
  };
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = createPasskey)]
    fn create_passkey_js(opts: JsValue) -> js_sys::Promise;
    #[wasm_bindgen(js_name = getPasskey)]
    fn get_passkey_js(opts: JsValue) -> js_sys::Promise;
}

async fn webauthn_create_credential(opts: serde_json::Value) -> Result<JsValue, JsValue> {
    let s = serde_json::to_string(&opts).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let js_opts = js_sys::JSON::parse(&s)?;
    JsFuture::from(create_passkey_js(js_opts)).await
}

async fn webauthn_get_credential(opts: serde_json::Value) -> Result<JsValue, JsValue> {
    let s = serde_json::to_string(&opts).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let js_opts = js_sys::JSON::parse(&s)?;
    JsFuture::from(get_passkey_js(js_opts)).await
}

#[component]
fn SecurityPage(user: RwSignal<Option<User>>) -> impl IntoView {
    let methods = RwSignal::new(Vec::<AuthMethod>::new());
    let sessions = RwSignal::new(Vec::<SessionRow>::new());
    let methods_status = RwSignal::new(String::new());
    let sessions_status = RwSignal::new(String::new());
    let passkey_pending = RwSignal::new(false);
    let passkey_error = RwSignal::new(None::<String>);
    let passkey_ok = RwSignal::new(None::<String>);
    let display_name = RwSignal::new(String::new());
    let show_stepup = RwSignal::new(false);
    let stepup_pw = RwSignal::new(String::new());
    let stepup_pending = RwSignal::new(false);
    let stepup_msg = RwSignal::new(None::<String>);
    let stepup_err = RwSignal::new(None::<String>);
    let lifecycle = Arc::new(AtomicBool::new(true));
    {
        let lifecycle = Arc::clone(&lifecycle);
        on_cleanup(move || lifecycle.store(false, Ordering::Release));
    }

    let load_methods = {
        let methods = methods;
        let methods_status = methods_status;
        let lifecycle = Arc::clone(&lifecycle);
        move || {
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                methods_status.set("Reading credential ledger...".into());
                match Request::get("/api/v1/auth/methods").send().await {
                    Ok(resp) if resp.ok() => match resp.json::<MethodsResponse>().await {
                        Ok(data) => {
                            if lifecycle.load(Ordering::Acquire) {
                                methods.set(data.methods);
                                methods_status.set(String::new());
                            }
                        }
                        Err(_) => methods_status.set("Credential response was not valid.".into()),
                    },
                    Ok(resp) => methods_status.set(api_error(&resp).await),
                    Err(_) => methods_status.set("Auth service did not answer.".into()),
                }
            });
        }
    };

    let load_sessions = {
        let sessions = sessions;
        let sessions_status = sessions_status;
        let lifecycle = Arc::clone(&lifecycle);
        move || {
            let lifecycle = Arc::clone(&lifecycle);
            spawn_local(async move {
                sessions_status.set("Reading sessions...".into());
                match Request::get("/api/v1/auth/sessions").send().await {
                    Ok(resp) if resp.ok() => match resp.json::<SessionsResponse>().await {
                        Ok(data) => {
                            if lifecycle.load(Ordering::Acquire) {
                                sessions.set(data.sessions);
                                sessions_status.set(String::new());
                            }
                        }
                        Err(_) => sessions_status.set("Session response was not valid.".into()),
                    },
                    Ok(resp) => sessions_status.set(api_error(&resp).await),
                    Err(_) => sessions_status.set("Session service did not answer.".into()),
                }
            });
        }
    };

    // initial load
    {
        let lm = load_methods.clone();
        let ls = load_sessions.clone();
        spawn_local(async move {
            lm();
            ls();
        });
    }

    let refresh = {
        let lm = load_methods.clone();
        let ls = load_sessions.clone();
        move |_| {
            lm();
            ls();
        }
    };

    let add_passkey = {
        let load_methods = load_methods.clone();
        move |_| {
            if passkey_pending.get_untracked() {
                return;
            }
            passkey_pending.set(true);
            passkey_error.set(None);
            passkey_ok.set(None);
            let display = display_name.get_untracked().trim().to_owned();
            let load_methods = load_methods.clone();
            spawn_local(async move {
                let outcome: Result<String, String> = async {
                    let body = if display.is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::json!({"display_name": display})
                    };
                    let start_resp = Request::post("/api/v1/auth/methods/webauthn/register")
                        .json(&body)
                        .map_err(|_| "Could not start registration.".to_string())?
                        .send()
                        .await
                        .map_err(|_| "Passkey service did not answer.".to_string())?;
                    if !start_resp.ok() {
                        return Err(api_error(&start_resp).await);
                    }
                    let started: WebauthnRegisterStart = start_resp
                        .json()
                        .await
                        .map_err(|_| "Invalid registration options.".to_string())?;
                    let opts = serde_json::to_value(started.creation_options)
                        .map_err(|_| "Invalid creation options.".to_string())?;
                    let credential_js = webauthn_create_credential(opts)
                        .await
                        .map_err(|e| map_webauthn_error(&e))?;
                    let credential = js_credential_to_json(credential_js)?;
                    let complete = Request::post("/api/v1/auth/methods/webauthn/complete")
                        .json(&serde_json::json!({
                            "ceremony_id": started.ceremony_id,
                            "credential": credential,
                        }))
                        .map_err(|_| "Could not encode credential.".to_string())?
                        .send()
                        .await
                        .map_err(|_| "Verification did not answer.".to_string())?;
                    if !complete.ok() {
                        return Err(api_error(&complete).await);
                    }
                    Ok("Passkey added.".into())
                }
                .await;
                match outcome {
                    Ok(msg) => {
                        passkey_ok.set(Some(msg));
                        load_methods();
                    }
                    Err(e) => passkey_error.set(Some(e)),
                }
                passkey_pending.set(false);
            });
        }
    };

    let delete_method = {
        let load_methods = load_methods.clone();
        move |id: String| {
            let load_methods = load_methods.clone();
            spawn_local(async move {
                match Request::delete(&format!("/api/v1/auth/methods/{id}"))
                    .send()
                    .await
                {
                    Ok(resp) if resp.ok() => load_methods(),
                    Ok(resp) => {
                        let msg = api_error(&resp).await;
                        passkey_error.set(Some(msg));
                    }
                    Err(_) => passkey_error.set(Some("Delete did not answer.".into())),
                }
            });
        }
    };

    let revoke_one = {
        let load_sessions = load_sessions.clone();
        move |hash: String| {
            let load_sessions = load_sessions.clone();
            spawn_local(async move {
                let resp = Request::post(&format!("/api/v1/auth/sessions/{hash}/revoke"))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.ok() => load_sessions(),
                    Ok(r) => sessions_status.set(api_error(&r).await),
                    Err(_) => sessions_status.set("Revoke did not answer.".into()),
                }
            });
        }
    };

    let revoke_others = {
        let load_sessions = load_sessions.clone();
        move |_| {
            let load_sessions = load_sessions.clone();
            spawn_local(async move {
                let resp = Request::post("/api/v1/auth/sessions/revoke").send().await;
                match resp {
                    Ok(r) if r.ok() => load_sessions(),
                    Ok(r) => sessions_status.set(api_error(&r).await),
                    Err(_) => sessions_status.set("Revoke did not answer.".into()),
                }
            });
        }
    };

    view! {
        <div class="page-heading">
            <div>
                <p class="utility">"IDENTITY / SECURITY"</p>
                <h1>"Security"</h1>
                <p class="form-note" style="max-width:720px">
                    "Keys, sessions, and re-authentication — the credential ledger. Passkeys are resident credentials verified by " <code>"webauthn-rs"</code> " with RP ID from public origin. One method must remain."
                </p>
            </div>
            <button class="secondary" on:click=refresh>"Refresh ledger"</button>
        </div>

        <section class="security-grid">
            <section class="security-section">
                <div class="section-heading">
                    <div>
                        <p class="utility">"CREDENTIALS"</p>
                        <h2>"Auth methods"</h2>
                    </div>
                    <span class="utility">{move || format!("{} methods", methods.get().len())}</span>
                </div>
                {move || {
                    let msg = methods_status.get();
                    (!msg.is_empty()).then(|| view! { <p class="form-note">{msg.clone()}</p> })
                }}
                {move || {
                    let list = methods.get();
                    if list.is_empty() {
                        view! {
                            <div class="security-empty">
                                <span class="index-spine">"—"</span>
                                <p class="form-note">"No credentials yet. Add a passkey or keep your password."</p>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="method-grid">
                                {list.into_iter().map(|m| {
                                    let can_delete = methods.get().len() > 1;
                                    let mid = m.id.clone();
                                    let mid2 = m.id.clone();
                                    let cls = method_spine_class(&m.method_type);
                                    let label = human_method_label(&m);
                                    let del = delete_method.clone();
                                    let is_primary = m.is_primary;
                                    let last = m.last_used_at.clone().unwrap_or_else(|| "never".into());
                                    view! {
                                        <article class=format!("method-card spine-{cls}")>
                                            <div class="method-head">
                                                <span class=format!("kind-badge {}", cls)>{label.clone()}</span>
                                                {is_primary.then(|| view!{ <span class="state-badge active">"PRIMARY"</span> })}
                                                <span class="mono-break" style="margin-left:auto;font-size:11px">{short_hash(&mid2)}</span>
                                            </div>
                                            <strong class="method-label">{m.label.clone().unwrap_or_else(|| label.clone())}</strong>
                                            <div class="method-meta">
                                                <span class="utility">"LAST USED · " {last}</span>
                                                <span class="utility">"CREATED · " {m.created_at.clone()}</span>
                                            </div>
                                            <div class="method-actions">
                                                <button class="secondary danger"
                                                    disabled=!can_delete
                                                    title=if can_delete { "Remove this method" } else { "Cannot remove the last method" }
                                                    on:click=move |_| del(mid.clone())>
                                                    "Remove"
                                                </button>
                                                {(!can_delete).then(|| view!{ <span class="form-note" style="font-size:12px">"Last method — add another before removing."</span> })}
                                            </div>
                                        </article>
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                }}
                <div class="passkey-add">
                    <label class="passkey-label">"New passkey label (optional)"
                        <input placeholder="e.g. MacBook Touch ID"
                            prop:value=move || display_name.get()
                            on:input=move |e| display_name.set(event_target_value(&e)) />
                    </label>
                    <button class="primary" disabled=move || passkey_pending.get() on:click=add_passkey>
                        {move || if passkey_pending.get() { "Waiting for authenticator..." } else { "Add passkey" }}
                    </button>
                    {move || passkey_error.get().map(|e| view!{ <p class="inline-error">{e}</p> })}
                    {move || passkey_ok.get().map(|o| view!{ <p class="test-success" style="padding:8px 10px;border:1px solid var(--teal);background:var(--teal-soft);border-radius:8px">{o}</p> })}
                    <p class="form-note">"Your browser will prompt for Touch ID, Windows Hello, or a security key. Ceremony expires in 5 minutes."</p>
                </div>
            </section>

            <section class="security-section">
                <div class="section-heading">
                    <div>
                        <p class="utility">"SESSIONS"</p>
                        <h2>"Active sessions"</h2>
                    </div>
                    <span class="utility">{move || format!("{} sessions", sessions.get().len())}</span>
                </div>
                {move || {
                    let msg = sessions_status.get();
                    (!msg.is_empty()).then(|| view!{ <p class="form-note">{msg.clone()}</p> })
                }}
                <div class="session-toolbar">
                    <p class="form-note">"Revoking signs the device out immediately. Current session stays until you sign out."</p>
                    <button class="secondary" disabled=move || sessions.get().len() <= 1 on:click=revoke_others>"Revoke others"</button>
                </div>
                {move || {
                    let list = sessions.get();
                    if list.is_empty() {
                        view!{ <p class="form-note">"No active sessions."</p> }.into_any()
                    } else {
                        view!{
                            <div class="session-list">
                                {list.into_iter().map(|s| {
                                    let hash = s.hash.clone();
                                    let h2 = s.hash.clone();
                                    let revoke = revoke_one.clone();
                                    let current = s.current;
                                    view!{
                                        <div class=if current { "session-row is-current" } else { "session-row" }>
                                            <span class="session-hash mono-break" title=hash.clone()>{short_hash(&hash)}</span>
                                            <span class="session-when">
                                                <span class="utility">"SEEN · " {s.last_seen_at.clone()}</span>
                                                <span class="utility">"EXPIRES · " {s.expires_at.clone()}</span>
                                            </span>
                                            <span class="session-badges">
                                                {current.then(|| view!{ <span class="state-badge active">"CURRENT"</span> })}
                                                {s.last_step_up_at.clone().map(|t| {
                                                    let title = t.clone();
                                                    let label = t.clone();
                                                    view!{ <span class="cap-chip" title=title>"step-up " {label}</span> }
                                                })}
                                            </span>
                                            <button class="secondary" disabled=current
                                                title=if current { "Current session — use Sign out" } else { "Revoke this session" }
                                                on:click=move |_| revoke(h2.clone())>
                                                "Revoke"
                                            </button>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                }}
            </section>

            <section class="security-section">
                <div class="section-heading">
                    <div>
                        <p class="utility">"STEP-UP"</p>
                        <h2>"Re-authenticate"</h2>
                    </div>
                    <button class="primary" on:click=move |_| { show_stepup.set(true); stepup_msg.set(None); stepup_err.set(None); }>"Re-authenticate"</button>
                </div>
                <p class="form-note">"Sensitive actions require a fresh password check. This marks the current session as step-up fresh for the next window."</p>
                {move || stepup_msg.get().map(|m| view!{ <p class="test-success" style="padding:10px;border:1px solid var(--teal);background:var(--teal-soft);border-radius:8px">{m}</p> })}
                {move || stepup_err.get().map(|e| view!{ <p class="inline-error">{e}</p> })}
                <div class="security-footer">
                    <span class="mono-break" style="font-size:11px;color:var(--slate)">"User: " {move || user.get().map(|u| format!("{} · {}", u.display_name, u.role)).unwrap_or_else(|| "—".into())}</span>
                    <button class="secondary" on:click=move |_| {
                        spawn_local(async move {
                            let _ = Request::post("/api/v1/auth/logout").send().await;
                            let _ = web_sys::window().and_then(|w| w.location().reload().ok());
                        });
                    }>"Sign out"</button>
                </div>
            </section>
        </section>

        {move || show_stepup.get().then(|| view!{
            <div class="modal-backdrop" on:click=move |e| {
                let target = e.target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok());
                let current = e.current_target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok());
                if target.is_some() && target == current { show_stepup.set(false); }
            }>
                <div class="modal" role="dialog" aria-modal="true" aria-label="Re-authenticate">
                    <div class="modal-head">
                        <h3>"Re-authenticate"</h3>
                        <button class="secondary" on:click=move |_| show_stepup.set(false)>"Close"</button>
                    </div>
                    <label>"Password"
                        <input type="password" autocomplete="current-password" placeholder="Enter your password"
                            prop:value=move || stepup_pw.get()
                            on:input=move |e| stepup_pw.set(event_target_value(&e)) />
                    </label>
                    {move || stepup_err.get().map(|e| view!{ <p class="inline-error">{e}</p> })}
                    {move || stepup_msg.get().map(|m| view!{ <p class="test-success">{m}</p> })}
                    <div class="stepper-actions">
                        <button class="secondary" on:click=move |_| show_stepup.set(false)>"Cancel"</button>
                        <button class="primary" disabled=move || stepup_pending.get() || stepup_pw.get().is_empty()
                            on:click=move |_| {
                                let pw = stepup_pw.get_untracked();
                                if pw.is_empty() || stepup_pending.get_untracked() {
                                    return;
                                }
                                stepup_pending.set(true);
                                stepup_err.set(None);
                                stepup_msg.set(None);
                                spawn_local(async move {
                                    let outcome = async {
                                        let r = Request::post("/api/v1/auth/step-up")
                                            .json(&serde_json::json!({"password": pw}))
                                            .map_err(|_| "Could not encode request.".to_string())?
                                            .send()
                                            .await
                                            .map_err(|_| "Service did not answer.".to_string())?;
                                        if r.ok() {
                                            Ok(())
                                        } else {
                                            Err(api_error(&r).await)
                                        }
                                    }
                                    .await;
                                    match outcome {
                                        Ok(()) => {
                                            stepup_msg.set(Some("Session is freshly authenticated.".into()));
                                            stepup_pw.set(String::new());
                                        }
                                        Err(e) => stepup_err.set(Some(e)),
                                    }
                                    stepup_pending.set(false);
                                });
                            }>
                            {move || if stepup_pending.get() { "Verifying..." } else { "Confirm" }}
                        </button>
                    </div>
                    <p class="form-note">"This does not change your password — it only proves possession for sensitive actions."</p>
                </div>
            </div>
        })}
    }
}

#[component]
fn EmptyOperationalPage(page: Page) -> impl IntoView {
    let (label, description) = match page {
        Page::Tasks => (
            "Tasks",
            "Durable work moves through explicit states and remains visible when agents run in the background.",
        ),
        Page::Agents => (
            "Agents",
            "Inspect every active agent, delegated scope, model, permission set, and run timeline.",
        ),
        Page::Terminals => (
            "Terminals",
            "Interactive shells open only inside configured sandbox environments and can reconnect by session ID.",
        ),
        Page::Skills => (
            "Skills",
            "Procedures are versioned, evaluated, and promoted with evidence rather than silent rewrites.",
        ),
        Page::Chat
        | Page::Library
        | Page::Autobiography
        | Page::Models
        | Page::Diagnostics
        | Page::Workspaces
        | Page::Mcp
        | Page::UiPackages
        | Page::Security => unreachable!(),
    };
    view! {
        <div class="page-heading"><div><p class="utility">"OPERATOR INDEX"</p><h1>{label}</h1></div></div>
        <div class="operational-empty"><span class="index-spine">"0"</span><h2>"Nothing indexed yet"</h2><p>{description}</p></div>
    }
}

fn load_conversation_summary(
    conversation_id: &str,
    task_generation: u64,
    generation: RwSignal<u64>,
    selected: RwSignal<Option<ConversationSummary>>,
    lifecycle: Arc<AtomicBool>,
) {
    let conversation_id = conversation_id.to_owned();
    spawn_local(async move {
        let response = Request::get(&format!("/api/v1/conversations/{conversation_id}"))
            .send()
            .await;
        if !conversation_task_is_current(
            &lifecycle,
            generation,
            task_generation,
            selected,
            &conversation_id,
        ) {
            return;
        }
        if let Ok(response) = response
            && response.ok()
        {
            let conversation = response.json::<ConversationSummary>().await;
            if conversation_task_is_current(
                &lifecycle,
                generation,
                task_generation,
                selected,
                &conversation_id,
            ) && let Ok(conversation) = conversation
            {
                selected.set(Some(conversation));
            }
        }
    });
}

fn resolve_active_run(
    user_id: String,
    conversation_id: String,
    task_generation: u64,
    signals: ChatTaskSignals,
    lifecycle: Arc<AtomicBool>,
) {
    let ChatTaskSignals {
        generation,
        selected,
        messages,
        streamed,
        tool_calls,
        active_run,
        resolving_run,
        status,
    } = signals;
    spawn_local(async move {
        let mut failures = 0_u32;
        loop {
            let response = Request::get(&format!("/api/v1/conversations/{conversation_id}/runs"))
                .send()
                .await;
            if !conversation_task_is_current(
                &lifecycle,
                generation,
                task_generation,
                selected,
                &conversation_id,
            ) {
                return;
            }
            match response {
                Ok(response) if response.ok() => {
                    let run = response.json::<Option<RunSummary>>().await;
                    if !conversation_task_is_current(
                        &lifecycle,
                        generation,
                        task_generation,
                        selected,
                        &conversation_id,
                    ) {
                        return;
                    }
                    match run {
                        Ok(Some(run)) => {
                            let active_session = ActiveRunSession {
                                user_id,
                                conversation_id,
                                run_id: run.id,
                            };
                            active_run.set(Some(active_session.run_id.clone()));
                            resolving_run.set(false);
                            if store_active_run(&active_session).is_ok() {
                                status.set("Model is responding...".into());
                            } else {
                                status.set(
                                    "Model is responding; reload recovery is unavailable.".into(),
                                );
                            }
                            follow_run(
                                active_session,
                                task_generation,
                                ChatTaskSignals {
                                    generation,
                                    selected,
                                    messages,
                                    streamed,
                                    tool_calls,
                                    active_run,
                                    resolving_run,
                                    status,
                                },
                                lifecycle,
                            );
                            return;
                        }
                        Ok(None) => {
                            clear_active_run_for_conversation(&user_id, &conversation_id);
                            active_run.set(None);
                            resolving_run.set(false);
                            status.set("Conversation ready.".into());
                            return;
                        }
                        Err(_) => {
                            failures = failures.saturating_add(1);
                            status.set(
                                "Active run response was invalid; retrying discovery...".into(),
                            );
                        }
                    }
                }
                Ok(response) => {
                    let response_status = response.status();
                    if is_terminal_poll_status(response_status) {
                        clear_active_run_for_conversation(&user_id, &conversation_id);
                        active_run.set(None);
                        resolving_run.set(false);
                        selected.set(None);
                        status.set(format!(
                            "Conversation access was lost (HTTP {response_status})."
                        ));
                        return;
                    }
                    failures = failures.saturating_add(1);
                    status.set(format!(
                        "Active run lookup failed: HTTP {response_status}; retrying discovery..."
                    ));
                }
                Err(_) => {
                    failures = failures.saturating_add(1);
                    status.set("Run service did not answer; retrying discovery...".into());
                }
            }
            let delay_ms = 250_i32.saturating_mul(1_i32 << failures.min(4));
            wait_for_poll(delay_ms.min(4_000)).await;
        }
    });
}

fn follow_run(
    active_session: ActiveRunSession,
    task_generation: u64,
    signals: ChatTaskSignals,
    lifecycle: Arc<AtomicBool>,
) {
    let ChatTaskSignals {
        generation,
        selected,
        messages,
        streamed,
        tool_calls,
        active_run,
        resolving_run,
        status,
    } = signals;
    let _ = &tool_calls;
    spawn_local(async move {
        let run_id = active_session.run_id.clone();
        let conversation_id = active_session.conversation_id.clone();
        let mut cursor = 0_i64;
        let mut event_failures = 0_u32;
        let mut run_failures = 0_u32;
        let mut terminal_event = None::<String>;
        loop {
            let recovering = event_failures > 0 || run_failures > 0;
            if !run_task_is_current(
                &lifecycle,
                generation,
                task_generation,
                selected,
                &conversation_id,
                active_run,
                &run_id,
            ) {
                return;
            }
            let mut events_loaded = true;
            loop {
                let events_response = Request::get(&format!(
                    "/api/v1/runs/{run_id}/events?after={cursor}&limit=200"
                ))
                .send()
                .await;
                if !run_task_is_current(
                    &lifecycle,
                    generation,
                    task_generation,
                    selected,
                    &conversation_id,
                    active_run,
                    &run_id,
                ) {
                    return;
                }
                match events_response {
                    Ok(response) if response.ok() => {
                        let events = response.json::<Vec<RunEvent>>().await;
                        if !run_task_is_current(
                            &lifecycle,
                            generation,
                            task_generation,
                            selected,
                            &conversation_id,
                            active_run,
                            &run_id,
                        ) {
                            return;
                        }
                        let events = match events {
                            Ok(events) => events,
                            Err(_) => {
                                status.set("Run events were invalid; retrying polling...".into());
                                events_loaded = false;
                                break;
                            }
                        };
                        let page_len = events.len();
                        for event in events {
                            cursor = cursor.max(event.sequence);
                            match event.event_type.as_str() {
                                "run.output_reset" => streamed.set(String::new()),
                                "model.text_delta" => {
                                    if let Some(text) = event.payload["text"].as_str() {
                                        streamed.update(|output| output.push_str(text));
                                    }
                                }
                                "run.completed" => terminal_event = Some("completed".into()),
                                "run.failed" => {
                                    streamed.set(String::new());
                                    terminal_event = Some("failed".into());
                                }
                                "run.canceled" => {
                                    streamed.set(String::new());
                                    terminal_event = Some("canceled".into());
                                }
                                _ => {}
                            }
                        }
                        if page_len < 200 {
                            break;
                        }
                    }
                    Ok(response) => {
                        let response_status = response.status();
                        if is_terminal_poll_status(response_status) {
                            clear_active_run_if_matching(&active_session);
                            active_run.set(None);
                            resolving_run.set(false);
                            status.set(format!(
                                "Run access was lost (HTTP {response_status}); polling stopped and the composer is available."
                            ));
                            return;
                        }
                        status.set(format!(
                            "Run events failed: HTTP {response_status}; retrying polling..."
                        ));
                        events_loaded = false;
                        break;
                    }
                    Err(_) => {
                        status.set("Run event service did not answer; retrying polling...".into());
                        events_loaded = false;
                        break;
                    }
                }
            }
            if events_loaded {
                event_failures = 0;
            } else {
                event_failures = event_failures.saturating_add(1);
            }
            let run_response = Request::get(&format!("/api/v1/runs/{run_id}")).send().await;
            if !run_task_is_current(
                &lifecycle,
                generation,
                task_generation,
                selected,
                &conversation_id,
                active_run,
                &run_id,
            ) {
                return;
            }
            let run = match run_response {
                Ok(response) if response.ok() => {
                    let run = response.json::<RunSummary>().await;
                    if !run_task_is_current(
                        &lifecycle,
                        generation,
                        task_generation,
                        selected,
                        &conversation_id,
                        active_run,
                        &run_id,
                    ) {
                        return;
                    }
                    match run {
                        Ok(run) => {
                            run_failures = 0;
                            Some(run)
                        }
                        Err(_) => {
                            run_failures = run_failures.saturating_add(1);
                            status.set("Run response was invalid; retrying polling...".into());
                            None
                        }
                    }
                }
                Ok(response) => {
                    let response_status = response.status();
                    if is_terminal_poll_status(response_status) {
                        clear_active_run_if_matching(&active_session);
                        active_run.set(None);
                        resolving_run.set(false);
                        status.set(format!(
                            "Run access was lost (HTTP {response_status}); polling stopped and the composer is available."
                        ));
                        return;
                    }
                    run_failures = run_failures.saturating_add(1);
                    status.set(format!(
                        "Run status failed: HTTP {response_status}; retrying polling..."
                    ));
                    None
                }
                Err(_) => {
                    run_failures = run_failures.saturating_add(1);
                    status.set("Run service did not answer; retrying polling...".into());
                    None
                }
            };
            if !run_task_is_current(
                &lifecycle,
                generation,
                task_generation,
                selected,
                &conversation_id,
                active_run,
                &run_id,
            ) {
                return;
            }
            if recovering && events_loaded && run.is_some() {
                status.set("Model is responding...".into());
            }
            if let Some(run) = run {
                if matches!(run.state.as_str(), "completed" | "failed" | "canceled")
                    && terminal_event.as_deref() != Some(run.state.as_str())
                {
                    status.set("Finalizing durable run events...".into());
                    wait_for_poll(250).await;
                    continue;
                }
                match run.state.as_str() {
                    "completed" => {
                        clear_active_run_if_matching(&active_session);
                        active_run.set(None);
                        resolving_run.set(false);
                        streamed.set(String::new());
                        status.set("Answer committed to the conversation index.".into());
                        load_messages(
                            &conversation_id,
                            task_generation,
                            generation,
                            selected,
                            messages,
                            status,
                            Arc::clone(&lifecycle),
                        );
                        return;
                    }
                    "failed" => {
                        clear_active_run_if_matching(&active_session);
                        streamed.set(String::new());
                        active_run.set(None);
                        resolving_run.set(false);
                        status.set(format!(
                            "Run failed: {}",
                            run.error_code.unwrap_or_else(|| "unknown error".into())
                        ));
                        return;
                    }
                    "canceled" => {
                        clear_active_run_if_matching(&active_session);
                        streamed.set(String::new());
                        active_run.set(None);
                        resolving_run.set(false);
                        status.set("Run canceled before publication.".into());
                        return;
                    }
                    _ => {}
                }
            }
            let poll_failures = event_failures.max(run_failures);
            let delay_ms = if poll_failures == 0 {
                250
            } else {
                250_i32.saturating_mul(1_i32 << poll_failures.min(4))
            };
            wait_for_poll(delay_ms.min(4_000)).await;
        }
    });
}

async fn wait_for_poll(delay_ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let callback = Closure::once_into_js(move || {
            let _ = resolve.call0(&wasm_bindgen::JsValue::NULL);
        });
        let _ = web_sys::window().and_then(|window| {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    callback.unchecked_ref(),
                    delay_ms,
                )
                .ok()
        });
    });
    let _ = JsFuture::from(promise).await;
}

fn load_messages(
    conversation_id: &str,
    task_generation: u64,
    generation: RwSignal<u64>,
    selected: RwSignal<Option<ConversationSummary>>,
    messages: RwSignal<Vec<MessageSummary>>,
    status: RwSignal<String>,
    lifecycle: Arc<AtomicBool>,
) {
    let conversation_id = conversation_id.to_owned();
    spawn_local(async move {
        let response = Request::get(&format!("/api/v1/conversations/{conversation_id}/messages"))
            .send()
            .await;
        if !conversation_task_is_current(
            &lifecycle,
            generation,
            task_generation,
            selected,
            &conversation_id,
        ) {
            return;
        }
        match response {
            Ok(response) if response.ok() => {
                let found = response.json::<Vec<MessageSummary>>().await;
                if !conversation_task_is_current(
                    &lifecycle,
                    generation,
                    task_generation,
                    selected,
                    &conversation_id,
                ) {
                    return;
                }
                match found {
                    Ok(found) => {
                        messages.set(found);
                        status.set("Durable transcript loaded.".into());
                    }
                    Err(_) => status.set("Transcript response was not valid.".into()),
                }
            }
            Ok(response) => {
                if conversation_task_is_current(
                    &lifecycle,
                    generation,
                    task_generation,
                    selected,
                    &conversation_id,
                ) {
                    status.set(format!("Transcript failed: HTTP {}", response.status()));
                }
            }
            Err(_) => {
                if conversation_task_is_current(
                    &lifecycle,
                    generation,
                    task_generation,
                    selected,
                    &conversation_id,
                ) {
                    status.set("Conversation service did not answer.".into());
                }
            }
        }
    });
}

fn advance_generation(generation: RwSignal<u64>) -> u64 {
    let next = generation.get_untracked().wrapping_add(1);
    generation.set(next);
    next
}

fn conversation_task_is_current(
    lifecycle: &Arc<AtomicBool>,
    generation: RwSignal<u64>,
    task_generation: u64,
    selected: RwSignal<Option<ConversationSummary>>,
    conversation_id: &str,
) -> bool {
    lifecycle_is_active(lifecycle)
        && generation.get_untracked() == task_generation
        && selected
            .get_untracked()
            .is_some_and(|conversation| conversation.id == conversation_id)
}

fn run_task_is_current(
    lifecycle: &Arc<AtomicBool>,
    generation: RwSignal<u64>,
    task_generation: u64,
    selected: RwSignal<Option<ConversationSummary>>,
    conversation_id: &str,
    active_run: RwSignal<Option<String>>,
    run_id: &str,
) -> bool {
    conversation_task_is_current(
        lifecycle,
        generation,
        task_generation,
        selected,
        conversation_id,
    ) && active_run
        .get_untracked()
        .is_some_and(|active| active == run_id)
}

fn lifecycle_is_active(lifecycle: &Arc<AtomicBool>) -> bool {
    lifecycle.load(Ordering::Acquire)
}

fn is_terminal_poll_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 404)
}

fn load_pending_submission(user_id: &str) -> Option<PendingSubmission> {
    let storage = web_sys::window()?.session_storage().ok()??;
    let value = storage.get_item(PENDING_SUBMISSION_KEY).ok()??;
    serde_json::from_str(&value)
        .ok()
        .filter(|submission: &PendingSubmission| submission.user_id == user_id)
}

fn store_pending_submission(submission: &PendingSubmission) -> Result<(), ()> {
    let storage = web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .ok_or(())?;
    let value = serde_json::to_string(submission).map_err(|_| ())?;
    storage
        .set_item(PENDING_SUBMISSION_KEY, &value)
        .map_err(|_| ())
}

fn clear_pending_submission_if_matching(submission: &PendingSubmission) {
    let Some(storage) =
        web_sys::window().and_then(|window| window.session_storage().ok().flatten())
    else {
        return;
    };
    let stored = storage
        .get_item(PENDING_SUBMISSION_KEY)
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_str::<PendingSubmission>(&value).ok());
    if stored.as_ref() == Some(submission) {
        let _ = storage.remove_item(PENDING_SUBMISSION_KEY);
    }
}

fn load_active_run(user_id: &str) -> Option<ActiveRunSession> {
    let storage = web_sys::window()?.session_storage().ok()??;
    let value = storage.get_item(ACTIVE_RUN_KEY).ok()??;
    serde_json::from_str(&value)
        .ok()
        .filter(|run: &ActiveRunSession| run.user_id == user_id)
}

fn store_active_run(run: &ActiveRunSession) -> Result<(), ()> {
    let storage = web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .ok_or(())?;
    let value = serde_json::to_string(run).map_err(|_| ())?;
    storage.set_item(ACTIVE_RUN_KEY, &value).map_err(|_| ())
}

fn clear_active_run_if_matching(run: &ActiveRunSession) {
    let Some(storage) =
        web_sys::window().and_then(|window| window.session_storage().ok().flatten())
    else {
        return;
    };
    let stored = storage
        .get_item(ACTIVE_RUN_KEY)
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_str::<ActiveRunSession>(&value).ok());
    if stored.as_ref() == Some(run) {
        let _ = storage.remove_item(ACTIVE_RUN_KEY);
    }
}

fn clear_active_run_for_conversation(user_id: &str, conversation_id: &str) {
    let Some(storage) =
        web_sys::window().and_then(|window| window.session_storage().ok().flatten())
    else {
        return;
    };
    let stored = storage
        .get_item(ACTIVE_RUN_KEY)
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_str::<ActiveRunSession>(&value).ok());
    if stored.is_some_and(|run| run.user_id == user_id && run.conversation_id == conversation_id) {
        let _ = storage.remove_item(ACTIVE_RUN_KEY);
    }
}

fn clear_chat_storage() {
    let Some(storage) =
        web_sys::window().and_then(|window| window.session_storage().ok().flatten())
    else {
        return;
    };
    let _ = storage.remove_item(PENDING_SUBMISSION_KEY);
    let _ = storage.remove_item(ACTIVE_RUN_KEY);
}

fn new_client_submission_id() -> Option<String> {
    let crypto = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("crypto")).ok()?;
    if let Ok(random_uuid) = js_sys::Reflect::get(&crypto, &JsValue::from_str("randomUUID"))
        && let Ok(random_uuid) = random_uuid.dyn_into::<js_sys::Function>()
        && let Ok(uuid) = random_uuid.call0(&crypto)
        && let Some(uuid) = uuid.as_string()
    {
        return Some(uuid);
    }

    let get_random_values =
        js_sys::Reflect::get(&crypto, &JsValue::from_str("getRandomValues")).ok()?;
    let get_random_values = get_random_values.dyn_into::<js_sys::Function>().ok()?;
    let random = js_sys::Uint8Array::new_with_length(16);
    get_random_values.call1(&crypto, random.as_ref()).ok()?;
    let mut bytes = [0_u8; 16];
    random.copy_to(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    ))
}

fn load_library_list(
    books: RwSignal<Vec<BookSummary>>,
    status: RwSignal<String>,
    _query: Option<String>,
) {
    status.set("Reading authorized index...".into());
    spawn_local(async move {
        match Request::get("/api/v1/library/books").send().await {
            Ok(response) if response.ok() => match response.json::<Vec<BookSummary>>().await {
                Ok(found) => {
                    let count = found.len();
                    books.set(found);
                    status.set(format!("{count} authorized Books loaded."));
                }
                Err(_) => status.set("Library response was not valid.".into()),
            },
            Ok(response) => status.set(format!(
                "Library request failed: {}",
                api_error(&response).await
            )),
            Err(_) => status.set("Library service did not answer.".into()),
        }
    });
}

/// Number of books currently visible under the active filter/search.
fn visible_book_count(
    all_books: RwSignal<Vec<BookSummary>>,
    search_hits: RwSignal<Option<Vec<BookSummary>>>,
    filter: RwSignal<LibraryFilter>,
) -> usize {
    match search_hits.get() {
        Some(hits) => hits.len(),
        None => {
            let kind_filter = filter.get();
            all_books
                .get()
                .into_iter()
                .filter(|book| kind_filter.matches(&book.kind))
                .count()
        }
    }
}

/// `GET /library/search?q=&kind=` — unified search across all book kinds.
async fn search_library(query: &str, kind: Option<&str>) -> Result<Vec<BookSummary>, String> {
    let encoded = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
    let mut endpoint = format!("/api/v1/library/search?q={encoded}");
    if let Some(kind) = kind {
        let encoded_kind =
            url::form_urlencoded::byte_serialize(kind.as_bytes()).collect::<String>();
        endpoint.push_str(&format!("&kind={encoded_kind}"));
    }
    match Request::get(&endpoint).send().await {
        Ok(response) if response.ok() => response
            .json::<Vec<BookSummary>>()
            .await
            .map_err(|_| "Library response was not valid.".to_owned()),
        Ok(response) => Err(api_error(&response).await),
        Err(_) => Err("Library service did not answer.".to_owned()),
    }
}

/// `POST /library/books/{id}/load` — kind-specific progressive load.
async fn load_library_book(book_id: &str) -> Result<LoadedBook, String> {
    match Request::post(&format!("/api/v1/library/books/{book_id}/load"))
        .send()
        .await
    {
        Ok(response) if response.ok() => response
            .json::<LoadedBook>()
            .await
            .map_err(|_| "Book response was not valid.".to_owned()),
        Ok(response) => Err(api_error(&response).await),
        Err(_) => Err("Library service did not answer.".to_owned()),
    }
}

fn load_workspaces(workspaces: RwSignal<Vec<WorkspaceSummary>>) {
    spawn_local(async move {
        if let Ok(response) = Request::get("/api/v1/workspaces").send().await
            && response.ok()
            && let Ok(list) = response.json::<Vec<WorkspaceSummary>>().await
        {
            workspaces.set(list);
        }
    });
}

/// Renders the error envelope `{message, correlation_id}` or a fallback.
/// `Response::json` borrows, so the caller can format without consuming.
async fn api_error(response: &gloo_net::http::Response) -> String {
    let status = response.status();
    response.json::<ApiError>().await.map_or_else(
        |_| format!("The request failed with HTTP {status}."),
        |api| format!("{} Reference: {}", api.message, api.correlation_id),
    )
}

/// Reads a checkbox's checked state from a change event.
fn event_target_checked(event: &leptos::ev::Event) -> bool {
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok())
        .is_some_and(|input| input.checked())
}

fn event_target_value(event: &leptos::ev::Event) -> String {
    event
        .target()
        .and_then(|target| {
            if let Ok(input) = target.clone().dyn_into::<web_sys::HtmlInputElement>() {
                return Some(input.value());
            }
            if let Ok(area) = target.clone().dyn_into::<web_sys::HtmlTextAreaElement>() {
                return Some(area.value());
            }
            if let Ok(select) = target.dyn_into::<web_sys::HtmlSelectElement>() {
                return Some(select.value());
            }
            None
        })
        .unwrap_or_default()
}
fn load_chat_models(
    models: RwSignal<Vec<ChatModelConfiguration>>,
    status: RwSignal<String>,
    lifecycle: Arc<AtomicBool>,
) {
    spawn_local(async move {
        let response = Request::get("/api/v1/models/chat").send().await;
        if !lifecycle_is_active(&lifecycle) {
            return;
        }
        match response {
            Ok(response) if response.ok() => {
                let found = response.json().await;
                if !lifecycle_is_active(&lifecycle) {
                    return;
                }
                match found {
                    Ok(found) => models.set(found),
                    Err(_) => status.set("Chat model response was not valid.".into()),
                }
            }
            Ok(response) => status.set(format!(
                "Chat model request failed: HTTP {}",
                response.status()
            )),
            Err(_) => status.set("Chat model registry did not answer.".into()),
        }
    });
}

fn activate_chat_model(
    model_id: String,
    models: RwSignal<Vec<ChatModelConfiguration>>,
    status: RwSignal<String>,
    lifecycle: Arc<AtomicBool>,
) {
    status.set("Activating chat route...".into());
    spawn_local(async move {
        let response = Request::post(&format!("/api/v1/models/chat/{model_id}/activate"))
            .send()
            .await;
        if !lifecycle_is_active(&lifecycle) {
            return;
        }
        match response {
            Ok(response) if response.ok() => {
                status.set("Chat route activated.".into());
                load_chat_models(models, status, Arc::clone(&lifecycle));
            }
            Ok(response) => status.set(format!("Activation failed: HTTP {}", response.status())),
            Err(_) => status.set("Chat model registry did not answer.".into()),
        }
    });
}

fn load_conversations(
    conversations: RwSignal<Vec<ConversationSummary>>,
    status: RwSignal<String>,
    lifecycle: Arc<AtomicBool>,
) {
    spawn_local(async move {
        let response = Request::get("/api/v1/conversations").send().await;
        if !lifecycle_is_active(&lifecycle) {
            return;
        }
        match response {
            Ok(response) if response.ok() => {
                let found = response.json().await;
                if !lifecycle_is_active(&lifecycle) {
                    return;
                }
                match found {
                    Ok(found) => {
                        conversations.set(found);
                        status.set("Durable conversations loaded.".into());
                    }
                    Err(_) => status.set("Conversation response was not valid.".into()),
                }
            }
            Ok(response) => status.set(format!(
                "Conversation request failed: HTTP {}",
                response.status()
            )),
            Err(_) => status.set("Conversation service did not answer.".into()),
        }
    });
}

fn load_configurations(
    configurations: RwSignal<Vec<EmbeddingConfiguration>>,
    status: RwSignal<String>,
) {
    spawn_local(async move {
        match Request::get("/api/v1/embeddings/configurations")
            .send()
            .await
        {
            Ok(response) if response.ok() => match response.json().await {
                Ok(found) => {
                    configurations.set(found);
                    status.set("Embedding registry loaded.".into());
                }
                Err(_) => status.set("Embedding registry response was not valid.".into()),
            },
            Ok(response) => status.set(format!(
                "Registry request failed: HTTP {}",
                response.status()
            )),
            Err(_) => status.set("Embedding registry did not answer.".into()),
        }
    });
}

fn load_jobs(jobs: RwSignal<Vec<EmbeddingJob>>, status: RwSignal<String>) {
    status.set("Reading durable queue...".into());
    spawn_local(async move {
        match Request::get("/api/v1/embeddings/jobs").send().await {
            Ok(response) if response.ok() => match response.json().await {
                Ok(found) => {
                    jobs.set(found);
                    status.set("Queue snapshot loaded.".into());
                }
                Err(_) => status.set("Queue response was not valid.".into()),
            },
            Ok(response) => status.set(format!("Queue request failed: HTTP {}", response.status())),
            Err(_) => status.set("Queue service did not answer.".into()),
        }
    });
}

fn load_autobiography(
    body: RwSignal<String>,
    revision: RwSignal<i64>,
    policy: RwSignal<String>,
    updated_at: RwSignal<String>,
    status: RwSignal<String>,
) {
    status.set("Reading Autobiography...".into());
    spawn_local(async move {
        match Request::get("/api/v1/autobiography").send().await {
            Ok(response) if response.ok() => match response.json::<AutobiographyResponse>().await {
                Ok(found) => {
                    body.set(found.body);
                    revision.set(found.revision);
                    policy.set(found.policy);
                    updated_at.set(found.updated_at);
                    status.set("Autobiography loaded.".into());
                }
                Err(_) => status.set("Autobiography response was not valid.".into()),
            },
            Ok(response) => status.set(format!(
                "Autobiography request failed: HTTP {}",
                response.status()
            )),
            Err(_) => status.set("Autobiography service did not answer.".into()),
        }
    });
}

fn load_auto_detect(
    detected: RwSignal<Vec<DetectedProviderInfo>>,
    detecting: RwSignal<bool>,
    status: RwSignal<String>,
) {
    detecting.set(true);
    status.set("Scanning for available providers...".into());
    spawn_local(async move {
        match Request::get("/api/v1/models/auto-detect").send().await {
            Ok(response) if response.ok() => match response.json::<AutoDetectResponse>().await {
                Ok(found) => {
                    detected.set(found.detected);
                    detecting.set(false);
                    status.set("Provider scan complete.".into());
                }
                Err(_) => {
                    detecting.set(false);
                    status.set("Auto-detect response was not valid.".into());
                }
            },
            Ok(response) => {
                detecting.set(false);
                status.set(format!(
                    "Auto-detect request failed: HTTP {}",
                    response.status()
                ))
            }
            Err(_) => {
                detecting.set(false);
                status.set("Auto-detect service did not answer.".into());
            }
        }
    });
}
