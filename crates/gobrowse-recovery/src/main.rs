#[cfg(target_arch = "wasm32")]
use gloo_net::http::Request;
#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use serde::{Deserialize, Serialize};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local;

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Deserialize)]
struct User {
    display_name: String,
    role: String,
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Deserialize)]
struct PackageItem {
    id: String,
    name: String,
    version: String,
    ui_kind: String,
    state: String,
    trust: String,
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Serialize)]
struct LoginReq<'a> {
    email: &'a str,
    password: &'a str,
}

#[cfg(target_arch = "wasm32")]
#[component]
fn RecoveryApp() -> impl IntoView {
    let (email, set_email) = signal(String::new());
    let (password, set_password) = signal(String::new());
    let (status, set_status) = signal(String::new());
    let (error, set_error) = signal(String::new());
    let (packages, set_packages) = signal(Vec::<PackageItem>::new());
    let (diagnostics, set_diagnostics) = signal(String::new());
    let (user, set_user) = signal(Option::<User>::None);

    let fetch_user = move || {
        spawn_local(async move {
            if let Ok(resp) = Request::get("/api/v1/auth/me").send().await
                && resp.ok()
                && let Ok(u) = resp.json::<User>().await
            {
                set_user.set(Some(u));
            }
        });
    };

    let fetch_packages = move || {
        spawn_local(async move {
            if let Ok(resp) = Request::get("/api/v1/ui/packages").send().await {
                if resp.ok() {
                    if let Ok(list) = resp.json::<Vec<PackageItem>>().await {
                        set_packages.set(list);
                    }
                } else {
                    set_error.set(format!("list failed: {}", resp.status()));
                }
            }
        });
    };

    let fetch_diag = move || {
        spawn_local(async move {
            let vers = Request::get("/api/v1/version").send().await.ok();
            let caps = Request::get("/api/v1/capabilities").send().await.ok();
            let mut out = String::new();
            if let Some(r) = vers
                && let Ok(text) = r.text().await
            {
                out.push_str(&format!("version: {text}\n"));
            }
            if let Some(r) = caps
                && let Ok(text) = r.text().await
            {
                out.push_str(&format!("capabilities: {text}\n"));
            }
            set_diagnostics.set(out);
        });
    };

    fetch_user();
    fetch_packages();
    fetch_diag();

    let do_login = move |_| {
        let e = email.get();
        let p = password.get();
        spawn_local(async move {
            let body = LoginReq {
                email: &e,
                password: &p,
            };
            match Request::post("/api/v1/auth/login")
                .json(&body)
                .unwrap()
                .send()
                .await
            {
                Ok(resp) if resp.ok() => {
                    set_status.set("Logged in".into());
                    set_error.set(String::new());
                    fetch_user();
                    fetch_packages();
                }
                Ok(resp) => {
                    let txt = resp.text().await.unwrap_or_else(|_| "login failed".into());
                    set_error.set(txt);
                }
                Err(err) => set_error.set(err.to_string()),
            }
        });
    };

    let do_activate = move |id: String| {
        let idc = id.clone();
        spawn_local(async move {
            match Request::post(&format!("/api/v1/ui/packages/{idc}/activate"))
                .send()
                .await
            {
                Ok(resp) if resp.ok() => {
                    set_status.set(format!("Activated {idc}"));
                    fetch_packages();
                }
                Ok(resp) => {
                    set_error.set(format!("activate failed: {}", resp.status()));
                }
                Err(e) => set_error.set(e.to_string()),
            }
        });
    };

    let do_rollback = move |_| {
        spawn_local(async move {
            match Request::post("/api/v1/ui/rollback").send().await {
                Ok(resp) if resp.ok() => {
                    set_status.set("Rolled back".into());
                    fetch_packages();
                }
                Ok(resp) => set_error.set(format!("rollback failed: {}", resp.status())),
                Err(e) => set_error.set(e.to_string()),
            }
        });
    };

    let do_delete = move |id: String| {
        let idc = id.clone();
        spawn_local(async move {
            match Request::delete(&format!("/api/v1/ui/packages/{idc}"))
                .send()
                .await
            {
                Ok(resp) if resp.status() == 204 => {
                    set_status.set(format!("Deleted {idc}"));
                    fetch_packages();
                }
                Ok(resp) => set_error.set(format!("delete failed: {}", resp.status())),
                Err(e) => set_error.set(e.to_string()),
            }
        });
    };

    view! {
        <header>
            <h1>"Gobrowse OS — Recovery"</h1>
            <span>"always-on minimal UI · /recovery"</span>
        </header>
        <main>
            <div class="card">
                <h2>"Session"</h2>
                {move || match user.get() {
                    Some(u) => view! { <p class="muted">"Logged in as " {u.display_name} " (" {u.role} ")"</p> }.into_any(),
                    None => view! {
                        <div class="grid">
                            <input type="email" placeholder="email" prop:value=move || email.get() on:input=move |ev| set_email.set(event_target_value(&ev)) />
                            <input type="password" placeholder="password" prop:value=move || password.get() on:input=move |ev| set_password.set(event_target_value(&ev)) />
                        </div>
                        <div class="row" style="margin-top:8px">
                            <button on:click=do_login>"Login"</button>
                        </div>
                    }.into_any()
                }}
                <p class="ok">{move || status.get()}</p>
                <p class="error">{move || error.get()}</p>
            </div>

            <div class="grid">
                <div class="card">
                    <h2>"UI Packages"</h2>
                    <div class="row" style="margin-bottom:8px">
                        <button on:click=move |_| fetch_packages()>"Refresh"</button>
                        <button class="secondary" on:click=do_rollback>"Rollback to previous"</button>
                    </div>
                    <div style="display:grid;gap:8px">
                        {move || packages.get().into_iter().map(|pkg| {
                            let id = pkg.id.clone();
                            let id2 = pkg.id.clone();
                            let badge_class = match pkg.state.as_str() {
                                "active" => "badge active",
                                "previous" => "badge previous",
                                _ => "badge",
                            };
                            view! {
                                <div class="item">
                                    <div class="row">
                                        <h3>{pkg.name.clone()} " v" {pkg.version.clone()}</h3>
                                        <span class=badge_class>{pkg.state.clone()}</span>
                                        <span class="badge">{pkg.ui_kind.clone()}</span>
                                        <span class="badge">{pkg.trust.clone()}</span>
                                    </div>
                                    <p class="muted">{pkg.id.clone()}</p>
                                    <div class="row">
                                        <button on:click=move |_| do_activate(id.clone())>"Activate"</button>
                                        <button class="secondary" on:click=move |_| do_delete(id2.clone())>"Delete"</button>
                                    </div>
                                </div>
                            }
                        }).collect_view()}
                        {move || if packages.get().is_empty() {
                            view! { <p class="muted">"No UI packages installed. Install via POST /api/v1/ui/preview + /install."</p> }.into_any()
                        } else {
                            view! { <span></span> }.into_any()
                        }}
                    </div>
                </div>

                <div class="card">
                    <h2>"Diagnostics"</h2>
                    <div class="row" style="margin-bottom:8px">
                        <button class="secondary" on:click=move |_| fetch_diag()>"Refresh"</button>
                    </div>
                    <pre>{move || diagnostics.get()}</pre>
                    <h2 style="margin-top:12px">"Restore Built-in"</h2>
                    <p class="muted">"Deactivate all UI packages and return to the built-in interface. Uses rollback if a previous exists, otherwise clears active."</p>
                    <button class="danger" on:click=do_rollback>"Restore built-in via rollback"</button>
                    <p class="muted" style="margin-top:8px">"If no previous, delete the active package after rollback fails (manual)."</p>
                </div>
            </div>

            <div class="card">
                <h2>"API Quick Reference"</h2>
                <pre>POST /api/v1/ui/preview  (source_type, source_uri, manifest) | POST /api/v1/ui/install  (source_type, source_uri, expected_digest, approve:true, manifest) | GET /api/v1/ui/packages | GET /api/v1/ui/packages/:id | POST /api/v1/ui/packages/:id/activate | POST /api/v1/ui/rollback | DELETE /api/v1/ui/packages/:id | GET /api/v1/capabilities | GET /recovery | GET /api/v1/ui/active-theme.css</pre>
                <p class="muted">"All UI endpoints are server-authoritative; CSP is computed server-side from asset hashes."</p>
            </div>
        </main>
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    leptos::mount::mount_to_body(RecoveryApp);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("gobrowse-recovery is built for wasm32-unknown-unknown with Trunk");
}
