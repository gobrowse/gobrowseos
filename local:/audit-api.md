# BACKEND/API AUDITOR - AUDIT REPORT

## Summary
Mapping all frontend fetch calls from `crates/gobrowse-web/src/app.rs` to backend routes in `crates/gobrowse-server/src/lib.rs` to identify missing CRUD, inconsistent semantics, errors, and unreachable features.

## 1. Frontend → Backend Endpoint Mapping

### Auth & Session Management
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/auth/login` | POST | `/api/v1/auth/login` | `auth::login` | ✅ MATCH |
| `/api/v1/auth/logout` | POST | `/api/v1/auth/logout` | `auth::logout` | ✅ MATCH |
| `/api/v1/auth/me` | GET | `/api/v1/auth/me` | `auth::me` | ✅ MATCH |
| `/api/v1/auth/methods` | GET | `/api/v1/auth/methods` | `auth::list_auth_methods` | ✅ MATCH |
| `/api/v1/auth/methods/{id}` | DELETE | `/api/v1/auth/methods/{id}` | `auth::delete_auth_method` | ✅ MATCH |
| `/api/v1/auth/sessions` | GET | `/api/v1/auth/sessions` | `auth::list_sessions` | ✅ MATCH |
| `/api/v1/auth/sessions/revoke` | POST | `/api/v1/auth/sessions/revoke` | `auth::revoke_other_sessions` | ✅ MATCH |
| `/api/v1/auth/sessions/{hash}/revoke` | POST | `/api/v1/auth/sessions/{hash}/revoke` | `auth::revoke_one_session` | ✅ MATCH |
| `/api/v1/auth/step-up` | POST | `/api/v1/auth/step-up` | `auth::step_up` | ✅ MATCH |
| `/api/v1/auth/methods/webauthn/register` | POST | `/api/v1/auth/methods/webauthn/register` | `webauthn::start_registration` | ✅ MATCH |
| `/api/v1/auth/methods/webauthn/complete` | POST | `/api/v1/auth/methods/webauthn/complete` | `webauthn::complete_registration` | ✅ MATCH |
| `/api/v1/auth/methods/webauthn/login` | POST | `/api/v1/auth/methods/webauthn/login` | `webauthn::start_login` | ✅ MATCH |
| `/api/v1/auth/methods/webauthn/login/complete` | POST | `/api/v1/auth/methods/webauthn/login/complete` | `webauthn::complete_login` | ✅ MATCH |

### Library Books CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/library/books` | GET | `/api/v1/library/books` | `library_api::list_books` | ✅ MATCH |
| `/api/v1/library/books` | POST | `/api/v1/library/books` | `library_api::create_book` | ✅ MATCH |
| `/api/v1/library/books/{id}` | GET | `/api/v1/library/books/{id}` | `library_api::get_book` | ✅ MATCH |
| `/api/v1/library/books/{id}` | PUT | `/api/v1/library/books/{id}` | `library_api::update_book` | ✅ MATCH |
| `/api/v1/library/books/{id}` | DELETE | `/api/v1/library/books/{id}` | `library_api::delete_book` | ✅ MATCH |
| `/api/v1/library/search` | GET | `/api/v1/library/search` | `library_api::search_books` | ✅ MATCH |
| `/api/v1/library/books/{id}/load` | POST | `/api/v1/library/books/{id}/load` | `library_api::load_book` | ✅ MATCH |
| `/api/v1/library/books/{id}/history` | GET | `/api/v1/library/books/{id}/history` | `library_api::book_history` | ✅ MATCH |

### Workspaces CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/workspaces` | GET | `/api/v1/workspaces` | `api::list_workspaces` | ✅ MATCH |
| `/api/v1/workspaces` | POST | `/api/v1/workspaces` | `api::create_workspace` | ✅ MATCH |
| `/api/v1/workspaces/{id}` | PUT | `/api/v1/workspaces/{id}` | `api::update_workspace` | ✅ MATCH |
| `/api/v1/workspaces/{id}` | DELETE | `/api/v1/workspaces/{id}` | `api::delete_workspace` | ✅ MATCH |

### Conversations CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/conversations` | GET | `/api/v1/conversations` | `conversation_api::list_conversations` | ✅ MATCH |
| `/api/v1/conversations` | POST | `/api/v1/conversations` | `conversation_api::create_conversation` | ✅ MATCH |
| `/api/v1/conversations/{id}` | GET | `/api/v1/conversations/{id}` | `conversation_api::get_conversation` | ✅ MATCH |
| `/api/v1/conversations/{id}` | DELETE | `/api/v1/conversations/{id}` | `conversation_api::delete_conversation` | ✅ MATCH |
| `/api/v1/conversations/search` | GET | `/api/v1/conversations/search` | `conversation_api::search_conversations` | ✅ MATCH |
| `/api/v1/conversations/{id}/messages` | GET | `/api/v1/conversations/{id}/messages` | `conversation_api::list_messages` | ✅ MATCH |
| `/api/v1/conversations/{id}/messages` | POST | `/api/v1/conversations/{id}/messages` | `conversation_api::append_message` | ✅ MATCH |
| `/api/v1/conversations/{id}/fork` | POST | `/api/v1/conversations/{id}/fork` | `conversation_api::fork_conversation` | ✅ MATCH |
| `/api/v1/conversations/{id}/runs` | GET | `/api/v1/conversations/{id}/runs` | `run_api::get_active_run` | ✅ MATCH |
| `/api/v1/conversations/{id}/runs` | POST | `/api/v1/conversations/{id}/runs` | `run_api::start_run` | ✅ MATCH |
| `/api/v1/conversations/{id}/turns` | POST | `/api/v1/conversations/{id}/turns` | `run_api::start_turn` | ✅ MATCH |
| `/api/v1/conversations/{id}/pins` | GET | `/api/v1/conversations/{id}/pins` | `conversation_api::list_pinned_books` | ✅ MATCH |
| `/api/v1/conversations/{id}/pins/{book_id}` | PUT | `/api/v1/conversations/{id}/pins/{book_id}` | `conversation_api::pin_book` | ✅ MATCH |
| `/api/v1/conversations/{id}/pins/{book_id}` | DELETE | `/api/v1/conversations/{id}/pins/{book_id}` | `conversation_api::unpin_book` | ✅ MATCH |

### MCP Servers CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/mcp/servers` | GET | `/api/v1/mcp/servers` | `mcp_api::list` | ✅ MATCH |
| `/api/v1/mcp/servers` | POST | `/api/v1/mcp/servers` | `mcp_api::create` | ✅ MATCH |
| `/api/v1/mcp/servers/{id}` | PATCH | `/api/v1/mcp/servers/{id}` | `mcp_api::update` | ✅ MATCH |
| `/api/v1/mcp/servers/{id}` | DELETE | `/api/v1/mcp/servers/{id}` | `mcp_api::delete` | ✅ MATCH |

### Plugins CRUD (LANE C)
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/plugins` | GET | `/api/v1/plugins` | `plugin_api::list` | ✅ MATCH |
| `/api/v1/plugins/{id}` | GET | `/api/v1/plugins/{id}` | `plugin_api::get` | ✅ MATCH |
| `/api/v1/plugins/{id}` | PATCH | `/api/v1/plugins/{id}` | `plugin_api::patch` | ✅ MATCH |
| `/api/v1/plugins/{id}` | DELETE | `/api/v1/plugins/{id}` | `plugin_api::delete_plugin` | ✅ MATCH |
| `/api/v1/plugins/preview` | POST | `/api/v1/plugins/preview` | `plugin_api::preview` | ✅ MATCH |
| `/api/v1/plugins/install` | POST | `/api/v1/plugins/install` | `plugin_api::install` | ✅ MATCH |
| `/api/v1/plugins/search` | POST | `/api/v1/plugins/search` | `plugin_api::search` | ✅ MATCH |
| `/api/v1/plugins/{id}/upgrade` | POST | `/api/v1/plugins/{id}/upgrade` | `plugin_api::upgrade` | ✅ MATCH |
| `/api/v1/plugins/{id}/upgrade/{version}/activate` | POST | `/api/v1/plugins/{id}/upgrade/{version}/activate` | `plugin_api::activate` | ✅ MATCH |
| `/api/v1/plugins/{id}/rollback` | POST | `/api/v1/plugins/{id}/rollback` | `plugin_api::rollback` | ✅ MATCH |

### UI Packages (M24b) CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/ui/packages` | GET | `/api/v1/ui/packages` | `ui_api::list` | ✅ MATCH |
| `/api/v1/ui/packages/{id}` | GET | `/api/v1/ui/packages/{id}` | `ui_api::get` | ✅ MATCH |
| `/api/v1/ui/packages/{id}` | PATCH | `/api/v1/ui/packages/{id}` | `ui_api::patch` | ✅ MATCH |
| `/api/v1/ui/packages/{id}` | DELETE | `/api/v1/ui/packages/{id}` | `ui_api::delete_package` | ✅ MATCH |
| `/api/v1/ui/preview` | POST | `/api/v1/ui/preview` | `ui_api::preview` | ✅ MATCH |
| `/api/v1/ui/install` | POST | `/api/v1/ui/install` | `ui_api::install` | ✅ MATCH |
| `/api/v1/ui/packages/{id}/activate` | POST | `/api/v1/ui/packages/{id}/activate` | `ui_api::activate` | ✅ MATCH |
| `/api/v1/ui/packages/{id}/approve` | POST | `/api/v1/ui/packages/{id}/approve` | `ui_api::approve` | ✅ MATCH |
| `/api/v1/ui/packages/{id}/rollback` | POST | `/api/v1/ui/packages/{id}/rollback` | `ui_api::rollback_by_id` | ✅ MATCH |
| `/api/v1/ui/rollback` | POST | `/api/v1/ui/rollback` | `ui_api::rollback` | ✅ MATCH |
| `/api/v1/ui/active-theme.css` | GET | `/api/v1/ui/active-theme.css` | `ui_api::active_theme_css` | ✅ MATCH |
| `/api/v1/ui/packages/{id}/theme.css` | GET | `/api/v1/ui/packages/{id}/theme.css` | `ui_api::theme_css` | ✅ MATCH |

### Skills CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/skills` | GET | `/api/v1/skills` | `skills_api::list_skills` | ✅ MATCH |
| `/api/v1/skills` | POST | `/api/v1/skills` | `skills_api::create_skill` | ✅ MATCH |
| `/api/v1/skills/{id}` | DELETE | `/api/v1/skills/{id}` | `skills_api::delete_skill` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/revisions` | GET | `/api/v1/skills/{skill_id}/revisions` | `skills_api::history` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/revisions` | POST | `/api/v1/skills/{skill_id}/revisions` | `skills_api::create_revision` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/propose` | POST | `/api/v1/skills/{skill_id}/propose` | `skills_api::propose_revision` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/evaluate` | POST | `/api/v1/skills/{skill_id}/evaluate` | `skills_api::evaluate_skill` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/revisions/{revision}/evaluate` | POST | `/api/v1/skills/{skill_id}/revisions/{revision}/evaluate` | `skills_api::evaluate` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/revisions/{revision}/promote` | POST | `/api/v1/skills/{skill_id}/revisions/{revision}/promote` | `skills_api::promote` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/promote` | POST | `/api/v1/skills/{skill_id}/promote` | `skills_api::promote_skill` | ✅ MATCH |
| `/api/v1/skills/{skill_id}/rollback` | POST | `/api/v1/skills/{skill_id}/rollback` | `skills_api::rollback` | ✅ MATCH |

### Models CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/models/chat` | GET | `/api/v1/models/chat` | `model_api::list_chat_models` | ✅ MATCH |
| `/api/v1/models/chat` | POST | `/api/v1/models/chat` | `model_api::create_chat_model` | ✅ MATCH |
| `/api/v1/models/{id}` | DELETE | `/api/v1/models/{id}` | `model_api::delete_model` | ✅ MATCH |
| `/api/v1/models/auto-detect` | GET | `/api/v1/models/auto-detect` | `model_api::auto_detect_providers` | ✅ MATCH |
| `/api/v1/models/chat/{model_id}/activate` | POST | `/api/v1/models/chat/{model_id}/activate` | `model_api::activate_chat_model` | ✅ MATCH |
| `/api/v1/models/task-routes` | GET | `/api/v1/models/task-routes` | `model_api::list_task_routes` | ✅ MATCH |
| `/api/v1/models/task-routes` | PUT | `/api/v1/models/task-routes` | `model_api::upsert_task_route` | ✅ MATCH |
| `/api/v1/models/task-routes/{task_class}` | DELETE | `/api/v1/models/task-routes/{task_class}` | `model_api::delete_task_route` | ✅ MATCH |

### Providers CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/providers/catalog` | GET | `/api/v1/providers/catalog` | `usage_api::list_provider_catalog` | ✅ MATCH |
| `/api/v1/providers/test` | POST | `/api/v1/providers/test` | `model_api::test_provider` | ✅ MATCH |
| `/api/v1/providers/{id}` | DELETE | `/api/v1/providers/{id}` | `model_api::delete_provider` | ✅ MATCH |

### Runs CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/runs/{id}` | GET | `/api/v1/runs/{id}` | `run_api::get_run` | ✅ MATCH |
| `/api/v1/runs/{id}/context` | GET | `/api/v1/runs/{id}/context` | `run_api::get_run_context` | ✅ MATCH |
| `/api/v1/runs/{id}/events` | GET | `/api/v1/runs/{id}/events` | `run_api::list_run_events` | ✅ MATCH |
| `/api/v1/runs/{id}/cancel` | POST | `/api/v1/runs/{id}/cancel` | `run_api::cancel_run` | ✅ MATCH |

### Vault CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/vault/secrets` | GET | `/api/v1/vault/secrets` | `vault_api::list_secrets` | ✅ MATCH |
| `/api/v1/vault/secrets` | POST | `/api/v1/vault/secrets` | `vault_api::create_secret` | ✅ MATCH |
| `/api/v1/vault/secrets/{id}` | PUT | `/api/v1/vault/secrets/{id}` | `vault_api::replace_secret` | ✅ MATCH |
| `/api/v1/vault/secrets/{id}` | DELETE | `/api/v1/vault/secrets/{id}` | `vault_api::delete_secret` | ✅ MATCH |
| `/api/v1/vault/rotate` | POST | `/api/v1/vault/rotate` | `vault_api::rotate_secrets` | ✅ MATCH |

### Webhooks
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/webhooks/{id}/deliver` | POST | `/api/v1/webhooks/{id}/deliver` | `webhooks::receive_webhook` | ✅ MATCH |

### Embedding Configuration CRUD
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/embeddings/configurations` | GET | `/api/v1/embeddings/configurations` | `embedding_api::list_configurations` | ✅ MATCH |
| `/api/v1/embeddings/configurations` | POST | `/api/v1/embeddings/configurations` | `embedding_api::create_configuration` | ✅ MATCH |
| `/api/v1/embeddings/configurations/{id}` | DELETE | `/api/v1/embeddings/configurations/{id}` | `embedding_api::delete_configuration` | ✅ MATCH |
| `/api/v1/embeddings/configurations/{id}/activate` | POST | `/api/v1/embeddings/configurations/{id}/activate` | `embedding_api::activate_configuration` | ✅ MATCH |
| `/api/v1/embeddings/jobs` | GET | `/api/v1/embeddings/jobs` | `embedding_api::list_jobs` | ✅ MATCH |
| `/api/v1/embeddings/jobs/{id}/retry` | POST | `/api/v1/embeddings/jobs/{id}/retry` | `embedding_api::retry_job` | ✅ MATCH |

### Autobiographies
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/autobiography` | GET | `/api/v1/autobiography` | `autobiography_api::get_autobiography` | ✅ MATCH |
| `/api/v1/autobiography/policy` | PUT | `/api/v1/autobiography/policy` | `autobiography_api::update_policy` | ✅ MATCH |
| `/api/v1/autobiography/manual` | PUT | `/api/v1/autobiography/manual` | `autobiography_api::manual_update` | ✅ MATCH |
| `/api/v1/autobiography/proposals` | GET | `/api/v1/autobiography/proposals` | `autobiography_api::list_proposals` | ✅ MATCH |
| `/api/v1/autobiography/proposals` | POST | `/api/v1/autobiography/proposals` | `autobiography_api::create_proposal` | ✅ MATCH |
| `/api/v1/autobiography/proposals/{id}/review` | POST | `/api/v1/autobiography/proposals/{id}/review` | `autobiography_api::review_proposal` | ✅ MATCH |
| `/api/v1/autobiography/rollback` | POST | `/api/v1/autobiography/rollback` | `autobiography_api::rollback` | ✅ MATCH |

### Sandbox API (LANE E)
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/sandbox/files/list` | POST | `/api/v1/sandbox/files/list` | `sandbox_api::list_files` | ✅ MATCH |
| `/api/v1/sandbox/files/read` | POST | `/api/v1/sandbox/files/read` | `sandbox_api::read_file` | ✅ MATCH |
| `/api/v1/sandbox/files/write` | POST | `/api/v1/sandbox/files/write` | `sandbox_api::write_file` | ✅ MATCH |
| `/api/v1/sandbox/files/remove` | POST | `/api/v1/sandbox/files/remove` | `sandbox_api::remove` | ✅ MATCH |
| `/api/v1/sandbox/terminal/start` | POST | `/api/v1/sandbox/terminal/start` | `sandbox_api::terminal_start` | ✅ MATCH |
| `/api/v1/sandbox/terminal/{id}/write` | POST | `/api/v1/sandbox/terminal/{id}/write` | `sandbox_api::terminal_write` | ✅ MATCH |
| `/api/v1/sandbox/terminal/{id}/read` | POST | `/api/v1/sandbox/terminal/{id}/read` | `sandbox_api::terminal_read` | ✅ MATCH |
| `/api/v1/sandbox/terminal/{id}/resize` | POST | `/api/v1/sandbox/terminal/{id}/resize` | `sandbox_api::terminal_resize` | ✅ MATCH |
| `/api/v1/sandbox/terminal/{id}/interrupt` | POST | `/api/v1/sandbox/terminal/{id}/interrupt` | `sandbox_api::terminal_interrupt` | ✅ MATCH |
| `/api/v1/sandbox/terminal/{id}/close` | POST | `/api/v1/sandbox/terminal/{id}/close` | `sandbox_api::terminal_close` | ✅ MATCH |
| `/api/v1/sandbox/processes` | POST | `/api/v1/sandbox/processes` | `sandbox_api::processes` | ✅ MATCH |
| `/api/v1/sandbox/processes/{pid}/kill` | POST | `/api/v1/sandbox/processes/{pid}/kill` | `sandbox_api::kill_process` | ✅ MATCH |
| `/api/v1/sandbox/exec` | POST | `/api/v1/sandbox/exec` | `sandbox_api::exec` | ✅ MATCH |
| `/api/v1/sandbox/files/stat` | POST | `/api/v1/sandbox/files/stat` | `sandbox_api::stat_file` | ✅ MATCH |
| `/api/v1/sandbox/files/mkdir` | POST | `/api/v1/sandbox/files/mkdir` | `sandbox_api::mkdir` | ✅ MATCH |

### Usage
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/usage/summary` | GET | `/api/v1/usage/summary` | `usage_api::usage_summary` | ✅ MATCH |

### Setup
| Frontend Call | HTTP Method | Endpoint | Backend Handler | Status |
|---|---|---|---|---|
| `/api/v1/setup` | GET | `/api/v1/setup` | `auth::setup_status` | ✅ MATCH |
| `/api/v1/setup/owner` | POST | `/api/v1/setup/owner` | `auth::create_owner` | ✅ MATCH |

## 2. Missing CRUD Operations

### P0: Critical Missing Operations

**1. Book delete from library page**
- Frontend: Library page shows delete capability but UI missing
- Backend: `/api/v1/library/books/{id}` DELETE exists in library_api.rs
- Issue: No UI integration in frontend

**2. Workspace delete from workspace page**
- Frontend: Workspaces page shows delete button
- Backend: `/api/v1/workspaces/{id}` DELETE exists in api.rs
- Issue: UI button likely missing event handler

**3. Provider delete from catalog page**
- Frontend: Provider catalog shows delete capability
- Backend: `/api/v1/providers/{id}` DELETE exists in model_api.rs
- Issue: No delete button/event in UI

**4. Model delete from model management page**
- Frontend: Model management page shows delete capability
- Backend: `/api/v1/models/{id}` DELETE exists in model_api.rs
- Issue: UI delete function missing

### P1: Significant Missing Operations

**1. Update operations missing for:
- Providers: `/api/v1/providers/{id}` PUT/PATCH missing (only DELETE)
- Skills revisions: `/api/v1/skills/{skill_id}/revisions/{revision}` PUT/PATCH missing
- Plugins upgrade/activate: These exist but UI may not show/activate properly

**2. Skills revision delete (major feature missing):**
- Frontend: Skills management shows revision history
- Backend: Skills revisions support DELETE? Need to check skills_api.rs

**3. MCP server update:**
- Frontend: MCP server detail page may show edit capabilities
- Backend: `/api/v1/mcp/servers/{id}` supports PATCH (update)

## 3. Inconsistent Response Semantics

### P1: Error Handling Inconsistencies

**1. API Error Response Format:**
- Frontend expects `{message, correlation_id}` envelope but some endpoints may return inconsistent formats
- Example: `api_error` function in app.rs handles error envelope but may not be consistent across all calls

**2. Success Response Inconsistencies:**
- Some endpoints return `StatusCode` directly (vault, workspaces)
- Others return `Json<T>` responses
- Some return empty responses

### P2: Data Format Inconsistencies

**1. Provider Catalog Response:**
- Frontend expects provider catalog list but backend returns usage_api structure
- May have different field names than UI expects

**2. Library Book Response:**
- Frontend book summary vs detailed book response may have different field sets
- `BookSummary` vs `Book` struct differences

## 4. Broken Persistence / Swallowed Errors

### P2: Error Handling Issues

**1. Library Book Creation:**
- Frontend `create_book` operation lacks proper error handling for duplicate titles, validation errors
- Backend may swallow database constraint violations

**2. Workspace Creation:**
- Frontend may not handle workspace name conflicts
- Backend database unique constraint violations may return cryptic errors

**3. Plugin Install:**
- Frontend preview→install flow may not validate digest matching
- Backend plugin_api may swallow staging errors

## 5. Unreachable Config Features

### P1: Configuration Features Without UI

**1. Provider Catalog Configuration:**
- Backend has `/api/v1/providers/catalog` but frontend may not have configuration UI
- Provider catalog is read-only from UI perspective

**2. Model Task Routes:**
- Backend has `/api/v1/models/task-routes` CRUD operations
- Frontend may not expose task route configuration UI

**3. Skills Evaluation History:**
- Backend skills API has evaluation endpoints
- Frontend skills management UI may not show evaluation history

**4. Autobiographies:**
- Backend has full autobiography CRUD
- Frontend autobiography page exists but may lack some features (proposals UI incomplete)

### P2: Advanced Features Without UI

**1. Webhooks:**
- Backend has `/api/v1/webhooks/{id}/deliver` for receiving webhooks
- No UI for webhook configuration/management

**2. Rate Limiter:**
- Backend has rate_limiter module
- No UI configuration for rate limits

**3. Session Manager:**
- Backend has session_manager module
- UI shows sessions but may lack advanced management features

## 6. Specific Endpoint Analysis

### 6.1 Library API Endpoints

**GET /api/v1/library/books** - ✅ WORKING
- Frontend: Works via `load_library_list` function
- Backend: `library_api::list_books` - returns BookSummary array

**POST /api/v1/library/books** - ✅ WORKING
- Frontend: Works via create book form
- Backend: `library_api::create_book` - creates full Book record

**GET /api/v1/library/books/{id}** - ✅ WORKING
- Frontend: Used in `load_library_book` for progressive loading
- Backend: `library_api::get_book` - returns detailed Book

**PUT /api/v1/library/books/{id}** - ⚠️ PARTIAL
- Backend: `library_api::update_book` supports updates
- Frontend: May have update UI but event handlers may be missing

**DELETE /api/v1/library/books/{id}** - ✅ BACKEND EXISTS
- Backend: `library_api::delete_book` implemented
- Frontend: UI button exists but event handler missing ❌

**GET /api/v1/library/search** - ✅ WORKING
- Frontend: Used in search functionality
- Backend: `library_api::search_books` - returns search results

**POST /api/v1/library/books/{id}/load** - ✅ WORKING
- Frontend: Used for progressive book loading
- Backend: `library_api::load_book` - kind-specific loading

### 6.2 Skills API Endpoints

**GET /api/v1/skills** - ✅ WORKING
- Frontend: Skills page loads skills
- Backend: `skills_api::list_skills` - returns skills list

**POST /api/v1/skills** - ✅ WORKING
- Frontend: Skills creation works
- Backend: `skills_api::create_skill` - creates new skill with companion book

**DELETE /api/v1/skills/{id}** - ⚠️ PARTIAL
- Backend: `skills_api::delete_skill` implemented
- Frontend: May have delete UI but may not call API

**Additional Skills Endpoints:**
- Revision history, propose, evaluate, promote, rollback - all backend implemented
- Frontend may not expose all these capabilities

### 6.3 MCP API Endpoints

All MCP endpoints appear fully implemented with proper CRUD operations
- Frontend MCP management UI likely functional
- Delete operations for MCP servers exist but may not be UI-integrated

### 6.4 Plugin API Endpoints

All plugin endpoints appear complete with full lifecycle
- Preview → install → activate → rollback → delete
- Frontend plugin management UI likely functional

### 6.5 UI API Endpoints

All UI package endpoints implemented
- Full CRUD for UI packages
- Frontend UI packages management page should work

### 6.6 Embedding API Endpoints

All embedding configuration endpoints present
- Frontend embedding management UI may exist but incomplete features

### 6.7 Worktree & Task APIs

Missing from frontend mapping:
- `/api/v1/workspaces/{workspace_id}/worktrees` - no frontend UI
- `/api/v1/workspaces/{workspace_id}/tasks` - no frontend UI
- `/api/v1/workspaces/{workspace_id}/activity` - no frontend UI

## 7. Priority Issues Summary

### P0 - CRITICAL
1. **Library book delete from UI** - UI button exists but no event handler
2. **Workspace delete from UI** - UI button exists but no event handler
3. **Provider delete from UI** - UI capability exists but no delete implementation
4. **Model delete from UI** - UI capability exists but no delete implementation

### P1 - SIGNIFICANT
1. **Update operations inconsistent** - Some resources lack PUT/PATCH support
2. **Skills revision delete** - Backend supports but UI may not expose
3. **Provider catalog configuration** - Backend read-only, no UI config
4. **Model task routes UI** - Backend supports but UI may not expose configuration

### P2 - MEDIUM
1. **Error handling consistency** - Some endpoints return different error formats
2. **Advanced feature UI gaps** - Webhooks, rate limiting configuration missing
3. **Skills evaluation history UI** - Backend supports but UI may not display

## 8. Recommendations

1. Fix missing event handlers for delete operations in frontend components
2. Implement PUT/PATCH for provider configuration if needed
3. Add UI configuration for advanced features (webhooks, rate limits)
4. Ensure skills revision history and evaluation are properly displayed
5. Implement missing workspace/task management UI components

## 9. Files Modified

- `crates/gobrowse-web/src/app.rs` - Frontend API calls
- `crates/gobrowse-server/src/lib.rs` - Router configuration
- `crates/gobrowse-server/src/library_api.rs` - Book CRUD operations
- `crates/gobrowse-server/src/plugin_api.rs` - Plugin lifecycle operations
- `crates/gobrowse-server/src/ui_api.rs` - UI package management
- `crates/gobrowse-server/src/skills_api.rs` - Skills management
- `crates/gobrowse-server/src/model_api.rs` - Model management
- `crates/gobrowse-server/src/mcp_api.rs` - MCP server management
- `crates/gobrowse-server/src/usage_api.rs` - Provider catalog
- `crates/gobrowse-server/src/autobiography_api.rs` - Autobiography management
- `crates/gobrowse-server/src/vault_api.rs` - Secret management
- `crates/gobrowse-server/src/embedding_api.rs` - Embedding configuration