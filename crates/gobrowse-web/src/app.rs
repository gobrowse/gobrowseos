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
    book_type: String,
    provenance: String,
    trust: String,
    retrieval_mode: String,
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

#[derive(serde::Deserialize, Clone, Debug)]
struct BookDetail {
    id: String,
    title: String,
    body: String,
    book_type: String,
    revision: i64,
}

#[derive(serde::Serialize)]
struct UpdateBookRequest {
    title: String,
    body: String,
    tags: Vec<String>,
    metadata: serde_json::Value,
    expected_revision: i64,
    reason: String,
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

#[derive(Debug, Serialize)]
struct CreateConversationRequest<'a> {
    title: &'a str,
    workspace_id: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
struct EmbeddingConfiguration {
    id: String,
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

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct SkillResponse {
    id: String,
    profile_id: String,
    workspace_id: Option<String>,
    name: String,
    description: String,
    active_revision: Option<i64>,
    promotion_policy: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct SkillRevisionResponse {
    id: String,
    skill_id: String,
    revision: i64,
    content: String,
    author: String,
    reason: String,
    source_conversation_ids: Vec<String>,
    created_at: String,
    evaluation: Option<serde_json::Value>,
    promoted: bool,
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

    view! {
        <section class="auth-layout">
            <div class="auth-thesis">
                <div class="brand-mark">"G/OS"</div>
                <p class="eyebrow">"Gobrowse OS / self-hosted agent environment"</p>
                <h1>"Keep the work. Find the context. Inspect every action."</h1>
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
                <NavGroup title="CONNECT" items=if is_admin { vec![("MCP", Page::Mcp), ("Models", Page::Models)] } else { vec![("MCP", Page::Mcp)] } page />
                {is_admin.then(|| view! { <NavGroup title="INSPECT" items=vec![("Diagnostics", Page::Diagnostics)] page /> })}
            </nav>
            <section id="workspace" class="workspace" tabindex="-1">
                {move || match page.get() {
                    Page::Chat => view! { <ChatPage user_id=user_id.clone() /> }.into_any(),
                    Page::Library => view! { <LibraryPage /> }.into_any(),
                    Page::Skills => view! { <SkillsPage /> }.into_any(),
                    Page::Autobiography => view! { <AutobiographyPage /> }.into_any(),
                    Page::Workspaces => view! { <WorkspacesPage /> }.into_any(),
                    Page::Mcp => view! { <McpPage /> }.into_any(),
                    Page::Models => view! { <ModelsPage /> }.into_any(),
                    Page::Diagnostics => view! { <DiagnosticsPage /> }.into_any(),
                    current => view! { <EmptyOperationalPage page=current /> }.into_any(),
                }}
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
    let active_run = RwSignal::new(restored_run.as_ref().map(|run| run.run_id.clone()));
    let resolving_run = RwSignal::new(restored_run.is_some());
    let submitting = RwSignal::new(false);
    let pending_submission = RwSignal::new(restored_submission);
    let generation = RwSignal::new(0_u64);
    let status = RwSignal::new(String::new());
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
        status.set("Creating conversation...".into());
        let lifecycle = Arc::clone(&create_lifecycle);
        spawn_local(async move {
            let request = Request::post("/api/v1/conversations").json(&CreateConversationRequest {
                title: value.trim(),
                workspace_id: None,
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
        let stored_run =
            load_active_run(&open_user_id).filter(|run| run.conversation_id == conversation_id);
        selected.set(Some(conversation));
        messages.set(Vec::new());
        streamed.set(String::new());
        active_run.set(stored_run.map(|run| run.run_id));
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
                active_run,
                resolving_run,
                status,
            },
            Arc::clone(&open_lifecycle),
        );
    };
    view! {
        <div class="page-heading">
            <div><p class="utility">"CONVERSATIONS / DURABLE"</p><h1>{move || selected.get().map_or_else(|| "What are we working on?".into(), |conversation| conversation.title)}</h1></div>
            <span class="model-chip">{move || chat_models.get().into_iter().find(|model| model.active).map_or_else(|| "NO ACTIVE MODEL".into(), |model| format!("{} / {}", model.provider_type.to_uppercase(), model.model_reference))}</span>
        </div>
        {move || if let Some(conversation) = selected.get() {
            let conversation_id = conversation.id.clone();
            let send = send.clone();
            let cancel = cancel.clone();
            view! {
                <div class="conversation-toolbar">
                    <button class="text-button" on:click=move |_| {
                        advance_generation(generation);
                        selected.set(None);
                        streamed.set(String::new());
                        active_run.set(None);
                        resolving_run.set(false);
                        submitting.set(false);
                    }>"← All conversations"</button>
                    <PinnedContext conversation_id=conversation_id.clone() />
                    <span class="utility">{format!("INDEX {}", &conversation_id[..8.min(conversation_id.len())])}</span>
                </div>
                <div class="transcript" aria-live="polite">
                    {move || messages.get().into_iter().map(|message| {
                        let provenance = match (&message.provider, &message.model) {
                            (Some(provider), Some(model)) => format!("{provider} / {model}"),
                            _ => "LOCAL USER".into(),
                        };
                        view! { <article class=if message.role == "assistant" { "message-entry assistant" } else { "message-entry user" }>
                            <div><span class="utility">{message.role.to_uppercase()}</span><small>{provenance}</small></div>
                            <p>{message.text}</p>
                        </article> }
                    }).collect_view()}
                    {move || (!streamed.get().is_empty()).then(|| view! {
                        <article class="message-entry assistant streaming">
                            <div><span class="utility">"ASSISTANT"</span><small>"STREAMING / DURABLE"</small></div>
                            <p>{streamed.get()}</p>
                        </article>
                    })}
                    {move || (messages.get().is_empty() && active_run.get().is_none() && !resolving_run.get()).then(|| view! {
                        <div class="conversation-empty"><span class="index-spine">"NEW"</span><div><h2>"Start the durable record"</h2><p>"Messages, selected context, model events, and the final answer remain inspectable after this session closes."</p></div></div>
                    })}
                </div>
                <form class="composer" on:submit=send>
                    <textarea required rows="4" maxlength="1000000" placeholder="Write the next message"
                        disabled=move || resolving_run.get() || active_run.get().is_some() || submitting.get()
                        prop:value=move || draft.get()
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
                            view! { <button class="primary" type="submit" disabled=move || submitting.get()>"Send + run"</button> }.into_any()
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
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct CreateWorkspaceBody {
    title: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct McpServerSummary {
    id: String,
    name: String,
    transport: String,
    configuration: serde_json::Value,
    enabled: bool,
    auth_secret_reference: Option<String>,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct CreateMcpServerBody {
    name: String,
    transport: String,
    configuration: serde_json::Value,
    enabled: bool,
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
            <div class="index-row header" role="row"><span>"ID"</span><span>"TITLE"</span><span>"DESCRIPTION"</span><span>"NETWORK POLICY"</span></div>
            {move || workspaces.get().into_iter().map(|ws| {
                let short_id = ws.id.chars().take(8).collect::<String>();
                view! { <div class="index-row" role="row">
                    <span class="spine-cell">{short_id}</span>
                    <strong>{ws.title}</strong>
                    <span>{ws.description}</span>
                    <span>{ws.network_policy.to_uppercase()}</span>
                </div> }
            }).collect_view()}
        </div>
    }
}

#[component]
fn McpPage() -> impl IntoView {
    let servers = RwSignal::new(Vec::<McpServerSummary>::new());
    let status = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let transport = RwSignal::new("stdio".to_owned());
    let configuration = RwSignal::new(String::new());

    let load = move || {
        status.set("Loading MCP servers...".into());
        spawn_local(async move {
            match Request::get("/api/v1/mcp/servers").send().await {
                Ok(response) if response.ok() => {
                    match response.json::<Vec<McpServerSummary>>().await {
                        Ok(list) => {
                            servers.set(list);
                            status.set(String::new());
                        }
                        Err(_) => status.set("MCP server response was not valid.".into()),
                    }
                }
                Ok(response) => status.set(format!(
                    "MCP server request failed: HTTP {}",
                    response.status()
                )),
                Err(_) => status.set("MCP server service did not answer.".into()),
            }
        });
    };

    load();

    let create = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let name_val = name.get_untracked().trim().to_owned();
        if name_val.is_empty() {
            status.set("Name is required.".into());
            return;
        }
        let transport_val = transport.get_untracked();
        let config_text = configuration.get_untracked().trim().to_owned();
        let config_value = if config_text.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str(&config_text) {
                Ok(v) => v,
                Err(e) => {
                    status.set(format!("Invalid configuration JSON: {e}"));
                    return;
                }
            }
        };
        status.set("Creating MCP server...".into());
        spawn_local(async move {
            let body = CreateMcpServerBody {
                name: name_val,
                transport: transport_val,
                configuration: config_value,
                enabled: true,
            };
            match Request::post("/api/v1/mcp/servers").json(&body) {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        name.set(String::new());
                        configuration.set(String::new());
                        load();
                    }
                    Ok(response) => {
                        status.set(format!("Create failed: HTTP {}", response.status()))
                    }
                    Err(_) => status.set("MCP server service did not answer.".into()),
                },
                Err(_) => status.set("Create request could not be encoded.".into()),
            }
        });
    };

    let delete_server = move |server_id: String| {
        spawn_local({
            let status = status;
            let load = load;
            async move {
                match Request::delete(&format!("/api/v1/mcp/servers/{server_id}"))
                    .send()
                    .await
                {
                    Ok(response) if response.ok() => load(),
                    Ok(response) => {
                        status.set(format!("Delete failed: HTTP {}", response.status()))
                    }
                    Err(_) => status.set("MCP server service did not answer.".into()),
                }
            }
        });
    };

    view! {
        <div class="page-heading">
            <div><p class="utility">"CONNECT / MCP"</p><h1>"MCP Servers"</h1></div>
            <span class="utility">{move || format!("{} SERVERS", servers.get().len())}</span>
        </div>
        <form class="filter-row" on:submit=create>
            <input placeholder="Server name" prop:value=move || name.get()
                on:input=move |event| name.set(event_target_value(&event)) />
            <select prop:value=move || transport.get()
                on:change=move |event| transport.set(event_target_value(&event))>
                <option value="stdio">"stdio"</option>
                <option value="streamable_http">"streamable_http"</option>
            </select>
            <textarea placeholder="Configuration (JSON)" prop:value=move || configuration.get()
                on:input=move |event| configuration.set(event_target_value(&event))></textarea>
            <button type="submit">"Add"</button>
        </form>
        <p class="form-note">{move || status.get()}</p>
        <div class="index-table" role="table">
            <div class="index-row header" role="row"><span>"NAME"</span><span>"TRANSPORT"</span><span>"STATE"</span><span>"ACTIONS"</span></div>
            {move || servers.get().into_iter().map(|server| {
                let sid = server.id.clone();
                view! { <div class="index-row" role="row">
                    <strong>{server.name}</strong>
                    <span>{server.transport.to_uppercase()}</span>
                    <span class="spine-cell">{if server.enabled { "ENABLED" } else { "DISABLED" }}</span>
                    <button class="text-button" on:click=move |_| delete_server(sid.clone())>"Delete"</button>
                </div> }
            }).collect_view()}
        </div>
    }
}

#[component]
fn LibraryPage() -> impl IntoView {
    let books = RwSignal::new(Vec::<BookSummary>::new());
    let query = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let selected = RwSignal::new(None::<BookDetail>);
    let edit_title = RwSignal::new(String::new());
    let edit_body = RwSignal::new(String::new());
    let edit_status = RwSignal::new(String::new());
    let create_title = RwSignal::new(String::new());
    let create_body = RwSignal::new(String::new());
    let create_status = RwSignal::new(String::new());
    let show_create = RwSignal::new(false);
    load_books(books, status, None);
    let create_book = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let title = create_title.get_untracked();
        if title.trim().is_empty() {
            create_status.set("Title is required".into());
            return;
        }
        let body = create_body.get_untracked();
        create_status.set("Creating book...".into());
        spawn_local(async move {
            let request = Request::post("/api/v1/library/books").json(&CreateBookBody {
                title: title.trim().to_owned(),
                body,
                book_type: "NOTE".into(),
                scope: "PROFILE".into(),
                tags: Vec::new(),
                provenance: "USER".into(),
                trust: "USER_PROVIDED".into(),
                workspace_id: None,
                conversation_id: None,
                security_classification: "INTERNAL".into(),
                metadata: serde_json::Value::Object(Default::default()),
            });
            match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        create_status.set("Created".into());
                        create_title.set(String::new());
                        create_body.set(String::new());
                        show_create.set(false);
                        load_books(books, status, None);
                    }
                    Ok(response) => create_status.set(format!("Create rejected: HTTP {}", response.status())),
                    Err(_) => create_status.set("Library service did not answer.".into()),
                },
                Err(_) => create_status.set("Create request could not be encoded.".into()),
            }
        });
    };
    let search = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = query.get_untracked();
        let query_value = (!value.trim().is_empty()).then(|| value.trim().to_owned());
        load_books(books, status, query_value);
    };
    let open_book = move |book_id: String| {
        spawn_local(async move {
            let response = Request::get(&format!("/api/v1/library/books/{book_id}"))
                .send()
                .await;
            match response {
                Ok(response) if response.ok() => {
                    if let Ok(detail) = response.json::<BookDetail>().await {
                        edit_title.set(detail.title.clone());
                        edit_body.set(detail.body.clone());
                        selected.set(Some(detail));
                    } else {
                        edit_status.set("Could not decode book".into());
                    }
                }
                Ok(response) => edit_status.set(format!("Book rejected: HTTP {}", response.status())),
                Err(_) => edit_status.set("Library service did not answer.".into()),
            }
        });
    };
    let save_book = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let Some(detail) = selected.get_untracked() else { return };
        let book_id = detail.id.clone();
        let title = edit_title.get_untracked();
        let body = edit_body.get_untracked();
        spawn_local(async move {
            let request = Request::put(&format!("/api/v1/library/books/{book_id}")).json(
                &UpdateBookRequest {
                    title: title.trim().to_owned(),
                    body: body.clone(),
                    tags: Vec::new(),
                    metadata: serde_json::Value::Object(Default::default()),
                    expected_revision: detail.revision,
                    reason: "Edited from the operator UI".into(),
                },
            );
            match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        edit_status.set("Saved".into());
                        selected.set(None);
                        load_books(books, status, None);
                    }
                    Ok(response) => edit_status.set(format!("Save rejected: HTTP {}", response.status())),
                    Err(_) => edit_status.set("Library service did not answer.".into()),
                },
                Err(_) => edit_status.set("Edit request could not be encoded.".into()),
            }
        });
    };
    view! {
        <div class="page-heading">
            <div><p class="utility">"GLOBAL CONTEXT / LIBRARY"</p><h1>"Books"</h1></div>
            <span class="utility">{move || format!("{} RESULTS", books.get().len())}</span>
        </div>
        <form class="filter-row" on:submit=search>
            <input placeholder="Lexical + semantic search" prop:value=move || query.get()
                on:input=move |event| query.set(event_target_value(&event)) />
            <button type="submit">"Search"</button>
            <button type="button" on:click=move |_| { query.set(String::new()); load_books(books, status, None); }>"Reset"</button>
            <button type="button" on:click=move |_| show_create.set(!show_create.get_untracked())>
                {move || if show_create.get() { "Close form" } else { "Create book" }}
            </button>
        </form>
        {move || show_create.get().then(|| view! {
            <form class="model-form" on:submit=create_book>
                <label>"Title"<input required maxlength="500" prop:value=move || create_title.get() on:input=move |event| create_title.set(event_target_value(&event)) /></label>
                <label class="fallback-field">"Body"
                    <textarea rows="8" maxlength="100000" placeholder="Book contents" prop:value=move || create_body.get() on:input=move |event| create_body.set(event_target_value(&event))></textarea>
                </label>
                <div>
                    <button class="primary" type="submit">"Create"</button>
                </div>
                <p class="form-note">{move || create_status.get()}</p>
            </form>
        })}
        <p class="form-note">{move || status.get()}</p>
        <div class="index-table" role="table">
            <div class="index-row header" role="row"><span>"ID"</span><span>"TITLE"</span><span>"PROVENANCE"</span><span>"RETRIEVAL"</span></div>
            {move || books.get().into_iter().map(|book| {
                let short_id = book.id.chars().take(8).collect::<String>();
                let book_id = book.id.clone();
                view! { <div class="index-row" role="row">
                    <span class="spine-cell">{short_id}</span>
                    <button class="text-button" on:click=move |_| open_book(book_id.clone())>{format!("{} / {}", book.title, book.book_type)}</button>
                    <span>{format!("{} · {}", book.provenance, book.trust)}</span>
                    <span>{book.retrieval_mode.to_uppercase()}</span>
                </div> }
            }).collect_view()}
        </div>
        {move || if let Some(detail) = selected.get() {
            let revision = detail.revision;
            let book_type = detail.book_type.clone();
            view! {
                <section class="model-section">
                    <div class="section-heading"><div><p class="utility">"BOOK / DETAIL"</p><h2>{format!("{} · rev {}", detail.title, revision)}</h2></div>
                        <span class="spine-cell">{book_type}</span></div>
                    <form class="model-form" on:submit=save_book>
                        <label>"Title"<input required maxlength="500" prop:value=move || edit_title.get() on:input=move |event| edit_title.set(event_target_value(&event)) /></label>
                        <label class="fallback-field">"Body"
                            <textarea rows="16" required maxlength="100000" prop:value=move || edit_body.get() on:input=move |event| edit_body.set(event_target_value(&event))></textarea>
                        </label>
                        <div>
                            <button class="primary" type="submit">"Save changes"</button>
                            <button class="text-button" type="button" on:click=move |_| { selected.set(None); }>"Close"</button>
                        </div>
                    </form>
                    <p class="form-note">{move || edit_status.get()}</p>
                </section>
            }.into_any()
        } else {
            view! { <span class="utility"></span> }.into_any()
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
        if let Ok(response) = response
            && response.ok()
            && let Ok(list) = response.json::<Vec<CatalogProvider>>().await
        {
            catalog.set(list.clone());
            // Populate the model select for the initial provider so the required
            // select has a matching option and the form can submit.
            if let Some(provider) = list
                .into_iter()
                .find(|entry| entry.provider_type == chat_provider.get_untracked())
            {
                catalog_models.set(provider.models.clone());
                if let Some(first) = provider.models.first() {
                    chat_model.set(first.reference.clone());
                    chat_context.set(first.context_window.to_string());
                    chat_output.set(first.output_limit.to_string());
                }
            }
        }
    });
    let on_provider_select = move |event: leptos::ev::Event| {
            let provider_type = event_target_value(&event);
            chat_provider.set(provider_type.clone());
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
            } else {
                catalog_models.set(Vec::new());
                chat_status.set("Choose a provider and model from the catalog".into());
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
    let detect = move |_| {
        if !detecting.get_untracked() {
            load_auto_detect(detected, detecting, status);
        }
    };
    let create = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let endpoint = base_url.get_untracked();
        let model_reference = model.get_untracked();
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
                    model_reference: model_reference.trim(),
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
        spawn_local(async move {
            if !lifecycle_is_active(&lifecycle) {
                return;
            }
            let fallback_model_ids = fallback_values.iter().map(String::as_str).collect();
            let request =
                Request::post("/api/v1/models/chat").json(&CreateChatModelConfiguration {
                    display_name: model_reference.trim(),
                    provider_type: provider.trim(),
                    base_url: endpoint.trim(),
                    secret_reference: (!secret.trim().is_empty()).then_some(secret.trim()),
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
            <form class="model-form" on:submit=create_chat>
                <label>"Provider type"
                    <select prop:value=move || chat_provider.get() on:change=on_provider_select>
                        <option value="">"Choose provider"</option>
                        {move || catalog.get().into_iter().map(|entry| {
                            let provider_type = entry.provider_type.clone();
                            let display = entry.display_name.clone();
                            view! { <option value=provider_type>{display}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <label>"Base URL"<input required prop:value=move || chat_base_url.get() on:input=move |event| chat_base_url.set(event_target_value(&event)) /></label>
                <label>"Model reference"
                    <select prop:value=move || chat_model.get() on:change=on_model_select>
                        <option value="">"Choose model"</option>
                        {move || catalog_models.get().into_iter().map(|entry| {
                            let display = entry.reference.clone();
                            let value = display.clone();
                            view! { <option value=value>{display}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <label>"Vault secret ID"<input placeholder="Optional" prop:value=move || chat_secret.get() on:input=move |event| chat_secret.set(event_target_value(&event)) /></label>
                <label>"Context window"<input required inputmode="numeric" prop:value=move || chat_context.get() on:input=move |event| chat_context.set(event_target_value(&event)) /></label>
                <label>"Output limit"<input required inputmode="numeric" prop:value=move || chat_output.get() on:input=move |event| chat_output.set(event_target_value(&event)) /></label>
                <label class="fallback-field">"Fallback model IDs"<input placeholder="Comma-separated, in failover order" prop:value=move || chat_fallbacks.get() on:input=move |event| chat_fallbacks.set(event_target_value(&event)) /></label>
                <button class="primary" type="submit">"Add + activate"</button>
            </form>
            <p class="form-note">{move || chat_status.get()}</p>
            <div class="index-table model-index">
                <div class="index-row header"><span>"STATE"</span><span>"MODEL"</span><span>"LIMITS"</span><span>"FALLBACKS"</span></div>
                {move || chat_configurations.get().into_iter().map(|configuration| {
                    let model_id = configuration.id.clone();
                    let activate_lifecycle = Arc::clone(&lifecycle);
                    view! { <div class="index-row"><span class="spine-cell">{if configuration.active { "ACTIVE" } else { "READY" }}</span>
                        <strong>{format!("{} / {}", configuration.display_name, configuration.model_reference)}</strong>
                        <span>{format!("{} · {}K / {}", configuration.provider_type, configuration.context_window / 1000, configuration.output_limit)}</span>
                        <button class="text-button" disabled=configuration.active on:click=move |_| activate_chat_model(model_id.clone(), chat_configurations, chat_status, Arc::clone(&activate_lifecycle))>
                            {if configuration.active { format!("{} FALLBACKS", configuration.fallback_model_ids.len()) } else { "Activate".into() }}
                        </button>
                    </div> }
                }).collect_view()}
            </div>
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
            <div class="index-row header"><span>"STATE"</span><span>"MODEL"</span><span>"PROVIDER"</span><span>"VECTOR"</span></div>
            {move || configurations.get().into_iter().map(|configuration| view! {
                <div class="index-row"><span class="spine-cell">{if configuration.active { "ACTIVE" } else { "READY" }}</span>
                <strong>{format!("{} / {}", configuration.display_name, configuration.model_reference)}</strong>
                <span>{format!("{} · {}", configuration.provider_type, &configuration.id[..8.min(configuration.id.len())])}</span>
                <span>{format!("{}D", configuration.dimensions)}</span></div>
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
                                if let Ok(summary) = response.json::<UsageSummary>().await {
                                    usage.set(Some(summary));
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
        let response = Request::get(&format!("/api/v1/usage/summary?window={}", window.get_untracked()))
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
            Ok(response) => status.set(format!("Usage endpoint rejected: HTTP {}", response.status())),
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
fn SkillsPage() -> impl IntoView {
    let skills = RwSignal::new(Vec::<SkillResponse>::new());
    let status = RwSignal::new(String::new());
    let expanded_skill = RwSignal::new(None::<String>);
    let revisions = RwSignal::new(Vec::<SkillRevisionResponse>::new());
    load_skills(skills, status);
    let toggle = move |skill_id: String| {
        let current = expanded_skill.get_untracked();
        if current.as_deref() == Some(&skill_id) {
            expanded_skill.set(None);
            revisions.set(Vec::new());
        } else {
            expanded_skill.set(Some(skill_id.clone()));
            load_skill_revisions(skill_id, revisions, status);
        }
    };
    view! {
        <div class="page-heading">
            <div><p class="utility">"PROCEDURES / SKILLS"</p><h1>"Skills"</h1></div>
            <span class="utility">{move || format!("{} SKILLS", skills.get().len())}</span>
        </div>
        <p class="form-note">{move || status.get()}</p>
        <div class="index-table" role="table">
            <div class="index-row header" role="row"><span>"ID"</span><span>"NAME"</span><span>"DESCRIPTION"</span><span>"REVISION"</span><span>"POLICY"</span></div>
            {move || skills.get().into_iter().map(|skill| {
                let skill_id = skill.id.clone();
                let short_id = skill_id.chars().take(8).collect::<String>();
                let is_expanded = {
                    let skill_id = skill_id.clone();
                    move || expanded_skill.get().as_deref() == Some(&skill_id)
                };
                let is_expanded_again = {
                    let skill_id = skill_id.clone();
                    move || expanded_skill.get().as_deref() == Some(&skill_id)
                };
                view! {
                    <>
                    <button class="index-row index-action" type="button" role="row"
                        class:active=is_expanded
                        on:click=move |_| toggle(skill_id.clone())>
                        <span class="spine-cell">{short_id}</span>
                        <strong>{skill.name}</strong>
                        <span class="utility">{skill.description}</span>
                        <span>{skill.active_revision.map_or("—".into(), |r| format!("v{r}"))}</span>
                        <span>{skill.promotion_policy.to_uppercase()}</span>
                    </button>
                    {move || is_expanded_again().then(|| {
                        let current_revisions = revisions.get();
                        view! {
                            <div class="index-table">
                                <div class="index-row header" role="row"><span>"REV"</span><span>"STATUS"</span><span>"CONTENT"</span><span>"REASON"</span></div>
                                {current_revisions.iter().map(|rev| {
                                    view! { <div class="index-row" role="row">
                                        <span class="spine-cell">{format!("v{}", rev.revision)}</span>
                                        <span>{if rev.promoted { "PROMOTED" } else { "DRAFT" }}</span>
                                        <pre>{rev.content.clone()}</pre>
                                        <span>{rev.reason.clone()}</span>
                                    </div> }
                                }).collect_view()}
                            </div>
                        }
                    })}
                    </>
                }
            }).collect_view()}
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
        | Page::Mcp => unreachable!(),
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
        active_run,
        resolving_run,
        status,
    } = signals;
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

fn load_books(books: RwSignal<Vec<BookSummary>>, status: RwSignal<String>, query: Option<String>) {
    status.set("Reading authorized index...".into());
    spawn_local(async move {
        let endpoint = query.map_or_else(
            || "/api/v1/library/books".to_owned(),
            |query| {
                let encoded =
                    url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
                format!("/api/v1/library/search?q={encoded}")
            },
        );
        match Request::get(&endpoint).send().await {
            Ok(response) if response.ok() => match response.json::<Vec<BookSummary>>().await {
                Ok(found) => {
                    let count = found.len();
                    books.set(found);
                    status.set(format!("{count} authorized Books loaded."));
                }
                Err(_) => status.set("Library response was not valid.".into()),
            },
            Ok(response) => status.set(format!(
                "Library request failed: HTTP {}",
                response.status()
            )),
            Err(_) => status.set("Library service did not answer.".into()),
        }
    });
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

fn load_skills(skills: RwSignal<Vec<SkillResponse>>, status: RwSignal<String>) {
    status.set("Reading Skills index...".into());
    spawn_local(async move {
        match Request::get("/api/v1/skills").send().await {
            Ok(response) if response.ok() => match response.json::<Vec<SkillResponse>>().await {
                Ok(found) => {
                    let count = found.len();
                    skills.set(found);
                    status.set(format!("{count} Skills loaded."));
                }
                Err(_) => status.set("Skills response was not valid.".into()),
            },
            Ok(response) => {
                status.set(format!("Skills request failed: HTTP {}", response.status()))
            }
            Err(_) => status.set("Skills service did not answer.".into()),
        }
    });
}

fn load_skill_revisions(
    skill_id: String,
    revisions: RwSignal<Vec<SkillRevisionResponse>>,
    status: RwSignal<String>,
) {
    status.set("Reading revision history...".into());
    spawn_local(async move {
        let endpoint = format!("/api/v1/skills/{skill_id}/revisions");
        match Request::get(&endpoint).send().await {
            Ok(response) if response.ok() => {
                match response.json::<Vec<SkillRevisionResponse>>().await {
                    Ok(found) => {
                        let count = found.len();
                        revisions.set(found);
                        status.set(format!("{count} revisions loaded."));
                    }
                    Err(_) => status.set("Revision response was not valid.".into()),
                }
            }
            Ok(response) => status.set(format!(
                "Revisions request failed: HTTP {}",
                response.status()
            )),
            Err(_) => status.set("Revisions service did not answer.".into()),
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
