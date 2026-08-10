use gloo_net::http::Request;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen_futures::spawn_local;

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

#[derive(Debug, Clone, Deserialize)]
struct ConversationSummary {
    id: String,
    title: String,
    status: String,
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
    let search_open = RwSignal::new(false);
    let is_admin = user
        .get_untracked()
        .is_some_and(|current| matches!(current.role.as_str(), "OWNER" | "ADMIN"));
    let logout = move |_| {
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
                <button class="library-search" on:click=move |_| search_open.set(true)>
                    <span>"Search the Library"</span><kbd>"/"</kbd>
                </button>
                <div class="runtime-status"><i></i><span>"SERVER READY"</span></div>
                <button class="user-control" on:click=logout>
                    {move || user.get().map_or_else(|| "Account".into(), |current| format!("{} · {}", current.display_name, current.role))}
                </button>
            </header>
            <nav class="side-nav" aria-label="Primary">
                <NavGroup title="OPERATE" items=vec![("Chat", Page::Chat), ("Tasks", Page::Tasks), ("Agents", Page::Agents), ("Terminals", Page::Terminals)] page />
                <NavGroup title="ORGANIZE" items=vec![("Library", Page::Library), ("Workspaces", Page::Workspaces), ("Skills", Page::Skills)] page />
                <NavGroup title="CONNECT" items=if is_admin { vec![("MCP", Page::Mcp), ("Models", Page::Models)] } else { vec![("MCP", Page::Mcp)] } page />
                {is_admin.then(|| view! { <NavGroup title="INSPECT" items=vec![("Diagnostics", Page::Diagnostics)] page /> })}
            </nav>
            <section id="workspace" class="workspace" tabindex="-1">
                {move || match page.get() {
                    Page::Chat => view! { <ChatPage /> }.into_any(),
                    Page::Library => view! { <LibraryPage /> }.into_any(),
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
        {move || search_open.get().then(|| view! {
            <dialog class="search-dialog" open>
                <form method="dialog" on:submit=move |_| search_open.set(false)>
                    <div class="search-header"><span>"LIBRARY INDEX"</span><button>"Close"</button></div>
                    <input autofocus placeholder="Search Books, conversations, messages, and Skills" />
                    <p>"Type a query to retrieve scoped lexical and semantic matches. Full content loads only when selected."</p>
                </form>
            </dialog>
        })}
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
fn ChatPage() -> impl IntoView {
    let conversations = RwSignal::new(Vec::<ConversationSummary>::new());
    let title = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    load_conversations(conversations, status);
    let create = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = title.get_untracked();
        if value.trim().is_empty() {
            return;
        }
        status.set("Creating conversation...".into());
        spawn_local(async move {
            let request = Request::post("/api/v1/conversations").json(&CreateConversationRequest {
                title: value.trim(),
                workspace_id: None,
            });
            match request {
                Ok(request) => match request.send().await {
                    Ok(response) if response.ok() => {
                        title.set(String::new());
                        load_conversations(conversations, status);
                    }
                    Ok(response) => {
                        status.set(format!("Create failed: HTTP {}", response.status()))
                    }
                    Err(_) => status.set("Conversation service did not answer.".into()),
                },
                Err(_) => status.set("Conversation request could not be encoded.".into()),
            }
        });
    };
    view! {
        <div class="page-heading">
            <div><p class="utility">"CONVERSATIONS / DURABLE"</p><h1>"What are we working on?"</h1></div>
            <button class="secondary">"Model: not configured"</button>
        </div>
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
                view! { <div class="index-row" role="row">
                    <span class="spine-cell">{short_id}</span><strong>{conversation.title}</strong>
                    <span>{conversation.status}</span><span>"MESSAGE + BOOK"</span>
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
    load_books(books, status, None);
    let search = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = query.get_untracked();
        let query_value = (!value.trim().is_empty()).then(|| value.trim().to_owned());
        load_books(books, status, query_value);
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
        </form>
        <p class="form-note">{move || status.get()}</p>
        <div class="index-table" role="table">
            <div class="index-row header" role="row"><span>"ID"</span><span>"TITLE"</span><span>"PROVENANCE"</span><span>"RETRIEVAL"</span></div>
            {move || books.get().into_iter().map(|book| {
                let short_id = book.id.chars().take(8).collect::<String>();
                view! { <div class="index-row" role="row">
                    <span class="spine-cell">{short_id}</span>
                    <strong>{format!("{} / {}", book.title, book.book_type)}</strong>
                    <span>{format!("{} · {}", book.provenance, book.trust)}</span>
                    <span>{book.retrieval_mode.to_uppercase()}</span>
                </div> }
            }).collect_view()}
        </div>
    }
}

#[component]
fn ModelsPage() -> impl IntoView {
    let configurations = RwSignal::new(Vec::<EmbeddingConfiguration>::new());
    let status = RwSignal::new(String::new());
    let base_url = RwSignal::new("http://127.0.0.1:11434".to_owned());
    let model = RwSignal::new("nomic-embed-text".to_owned());
    let dimensions = RwSignal::new("768".to_owned());
    load_configurations(configurations, status);
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
    view! {
        <div class="page-heading"><div><p class="utility">"MODELS / EMBEDDINGS"</p><h1>"Embedding registry"</h1></div></div>
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
    }
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
fn EmptyOperationalPage(page: Page) -> impl IntoView {
    let (label, description, action) = match page {
        Page::Workspaces => (
            "Workspaces",
            "Repositories, Books, tasks, agents, and persistent environments share one workspace boundary.",
            "Create workspace",
        ),
        Page::Tasks => (
            "Tasks",
            "Durable work moves through explicit states and remains visible when agents run in the background.",
            "Create task",
        ),
        Page::Agents => (
            "Agents",
            "Inspect every active agent, delegated scope, model, permission set, and run timeline.",
            "Create agent",
        ),
        Page::Terminals => (
            "Terminals",
            "Interactive shells open only inside configured sandbox environments and can reconnect by session ID.",
            "Enable sandbox",
        ),
        Page::Skills => (
            "Skills",
            "Procedures are versioned, evaluated, and promoted with evidence rather than silent rewrites.",
            "Import Skill",
        ),
        Page::Mcp => (
            "MCP",
            "Connect tools, resources, and prompts through versioned transports and credential references.",
            "Add MCP server",
        ),
        Page::Chat | Page::Library | Page::Models | Page::Diagnostics => unreachable!(),
    };
    view! {
        <div class="page-heading"><div><p class="utility">"OPERATOR INDEX"</p><h1>{label}</h1></div></div>
        <div class="operational-empty"><span class="index-spine">"0"</span><h2>"Nothing indexed yet"</h2><p>{description}</p><button class="primary">{action}</button></div>
    }
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

fn load_conversations(conversations: RwSignal<Vec<ConversationSummary>>, status: RwSignal<String>) {
    spawn_local(async move {
        match Request::get("/api/v1/conversations").send().await {
            Ok(response) if response.ok() => match response.json().await {
                Ok(found) => {
                    conversations.set(found);
                    status.set("Durable conversations loaded.".into());
                }
                Err(_) => status.set("Conversation response was not valid.".into()),
            },
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
