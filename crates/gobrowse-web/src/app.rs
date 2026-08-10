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
                <NavGroup title="CONNECT" items=vec![("MCP", Page::Mcp), ("Models", Page::Models)] page />
                <NavGroup title="INSPECT" items=vec![("Diagnostics", Page::Diagnostics)] page />
            </nav>
            <section id="workspace" class="workspace" tabindex="-1">
                {move || match page.get() {
                    Page::Chat => view! { <ChatPage /> }.into_any(),
                    Page::Library => view! { <LibraryPage /> }.into_any(),
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
                <button on:click=move |_| page.set(Page::Diagnostics)>"More"</button>
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
    view! {
        <div class="page-heading">
            <div><p class="utility">"CONVERSATION / NEW"</p><h1>"What are we working on?"</h1></div>
            <button class="secondary">"Model: not configured"</button>
        </div>
        <div class="conversation-empty">
            <div class="index-spine">"C-NEW"</div>
            <div>
                <h2>"Start with a goal, not a prompt."</h2>
                <p>"Gobrowse OS will assemble bounded context from this conversation, selected Books, workspace state, and relevant Skills. Sensitive tools still require policy approval."</p>
            </div>
        </div>
        <form class="composer">
            <textarea rows="4" placeholder="Describe the outcome, constraints, and what the agent may change."></textarea>
            <div><span>"No workspace · No model · 0 pinned Books"</span><button class="primary" disabled>"Run agent"</button></div>
        </form>
    }
}

#[component]
fn LibraryPage() -> impl IntoView {
    view! {
        <div class="page-heading">
            <div><p class="utility">"GLOBAL CONTEXT / LIBRARY"</p><h1>"Books"</h1></div>
            <button class="primary">"Create Book"</button>
        </div>
        <div class="filter-row"><input placeholder="Filter this index" /><button>"Scope: all"</button><button>"Trust: all"</button></div>
        <div class="index-table" role="table">
            <div class="index-row header" role="row"><span>"ID"</span><span>"TITLE"</span><span>"PROVENANCE"</span><span>"UPDATED"</span></div>
            <div class="index-row" role="row"><span class="spine-cell">"L-0001"</span><strong>"Autobiography"</strong><span>"USER · VERIFIED"</span><span>"just now"</span></div>
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
        Page::Models => (
            "Models",
            "Chat, embedding, reranking, image, and speech providers are configured independently.",
            "Add provider",
        ),
        Page::Diagnostics => (
            "Diagnostics",
            "Run focused checks without exposing credentials or raw prompts.",
            "Run doctor",
        ),
        Page::Chat | Page::Library => unreachable!(),
    };
    view! {
        <div class="page-heading"><div><p class="utility">"OPERATOR INDEX"</p><h1>{label}</h1></div></div>
        <div class="operational-empty"><span class="index-spine">"0"</span><h2>"Nothing indexed yet"</h2><p>{description}</p><button class="primary">{action}</button></div>
    }
}
