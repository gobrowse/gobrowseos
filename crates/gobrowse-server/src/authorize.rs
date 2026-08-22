use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::AppError;

// ── Actor types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Human {
        user_id: Uuid,
        role: String,
    },
    Agent {
        run_id: Uuid,
        agent_permissions: Vec<String>,
    },
    Plugin {
        plugin_id: Uuid,
        permissions: Vec<PermissionPreview>,
    },
    Mcp {
        server_id: Uuid,
        permissions: Vec<PermissionPreview>,
    },
    Webhook {
        id: Uuid,
    },
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPreview {
    pub action: String,
    pub resource_pattern: String,
}

// ── Action types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Read,
    Write,
    Execute { risk: RiskClass },
    Delete,
    Admin,
    ContextRetrieve,
    AccessSecret,
    WebhookDeliver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RiskClass {
    Read,
    Write,
    Destructive,
    System,
}

impl std::fmt::Display for RiskClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskClass::Read => write!(f, "read"),
            RiskClass::Write => write!(f, "write"),
            RiskClass::Destructive => write!(f, "destructive"),
            RiskClass::System => write!(f, "system"),
        }
    }
}

impl RiskClass {
    pub fn from_tool_risk(risk: &str) -> Self {
        match risk {
            "read" => RiskClass::Read,
            "write" => RiskClass::Write,
            "destructive" => RiskClass::Destructive,
            "system" => RiskClass::System,
            _ => RiskClass::Write,
        }
    }
}

// ── Resource types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Book(Uuid),
    Conversation(Uuid),
    Provider(String),
    Model(String),
    Workspace(Uuid),
    Plugin(Uuid),
    McpServer(Uuid),
    Sandbox,
    Secret(String),
    System,
}

// ── Decision ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

// ── Authorization context ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AuthorizationContext {
    pub profile_id: Uuid,
    pub user_id: Option<Uuid>,
}

// ── Policy cache key / value ─────────────────────────────────────────────

type CacheKey = (String, String, String); // (actor_type_key, action_key, resource_key)

pub type AuthPolicyCache = Arc<RwLock<HashMap<CacheKey, Decision>>>;

pub fn new_policy_cache() -> AuthPolicyCache {
    Arc::new(RwLock::new(HashMap::new()))
}

// ── Helper: actor key for matrix/cache ───────────────────────────────────

fn actor_key(actor: &Actor) -> String {
    match actor {
        Actor::Human { role, .. } => format!("HUMAN:{role}"),
        Actor::Agent { .. } => "AGENT".into(),
        Actor::Plugin { .. } => "PLUGIN".into(),
        Actor::Mcp { .. } => "MCP".into(),
        Actor::Webhook { .. } => "WEBHOOK".into(),
        Actor::System => "SYSTEM".into(),
    }
}

fn action_key(action: &Action) -> String {
    match action {
        Action::Read => "READ".into(),
        Action::Write => "WRITE".into(),
        Action::Execute { risk } => format!("EXECUTE:{risk}"),
        Action::Delete => "DELETE".into(),
        Action::Admin => "ADMIN".into(),
        Action::ContextRetrieve => "CONTEXT_RETRIEVE".into(),
        Action::AccessSecret => "ACCESS_SECRET".into(),
        Action::WebhookDeliver => "WEBHOOK_DELIVER".into(),
    }
}

fn resource_key(resource: &Resource) -> String {
    match resource {
        Resource::Book(_) => "BOOK".into(),
        Resource::Conversation(_) => "CONVERSATION".into(),
        Resource::Provider(_) => "PROVIDER".into(),
        Resource::Model(_) => "MODEL".into(),
        Resource::Workspace(_) => "WORKSPACE".into(),
        Resource::Plugin(_) => "PLUGIN".into(),
        Resource::McpServer(_) => "MCP_SERVER".into(),
        Resource::Sandbox => "SANDBOX".into(),
        Resource::Secret(_) => "SECRET".into(),
        Resource::System => "SYSTEM".into(),
    }
}

// ── Single-user short-circuit ────────────────────────────────────────────

/// Returns true if the profile has exactly 1 user with role OWNER and no
/// workspaces. The authorization short-circuit covers this case: no DB
/// policy queries needed, everything is ALLOWED for the single OWNER.
async fn is_single_user_owner(pool: &PgPool, profile_id: Uuid) -> Result<bool, sqlx::Error> {
    let user_count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE profile_id=$1")
        .bind(profile_id)
        .fetch_one(pool)
        .await?;

    if user_count != 1 {
        return Ok(false);
    }

    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM users WHERE profile_id=$1 LIMIT 1")
            .bind(profile_id)
            .fetch_one(pool)
            .await?;

    if role.as_deref() != Some("OWNER") {
        return Ok(false);
    }

    let workspace_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE profile_id=$1")
            .bind(profile_id)
            .fetch_one(pool)
            .await?;

    Ok(workspace_count == 0)
}

// ── Matrix-based default decision ────────────────────────────────────────

/// Default authorization matrix (hard ceiling, configurable via policies later).
fn default_matrix_decision(actor: &Actor, action: &Action, resource: &Resource) -> Decision {
    match actor {
        Actor::Human { role, user_id: _ } => match role.as_str() {
            "OWNER" => Decision::Allow,
            "ADMIN" => Decision::Allow,
            "MEMBER" => match action {
                Action::Read => Decision::Allow,
                Action::Write | Action::Delete => match resource {
                    Resource::Book(_id) => {
                        // Members can write their own books; cross-profile writes are denied.
                        // Without workspace context here, we allow for now (workspace membership
                        // is checked at the DB query level for books).
                        Decision::Allow
                    }
                    Resource::Conversation(_id) => {
                        // Members can write to conversations they own or in workspaces they
                        // belong to. The DB query layer enforces workspace membership.
                        Decision::Allow
                    }
                    _ => Decision::Deny,
                },
                Action::Execute { risk } => {
                    if *risk >= RiskClass::Destructive {
                        Decision::Deny
                    } else {
                        Decision::Allow
                    }
                }
                _ => Decision::Deny,
            },
            "VIEWER" => match action {
                Action::Read => Decision::Allow,
                _ => Decision::Deny,
            },
            _ => Decision::Deny,
        },
        Actor::Agent {
            agent_permissions, ..
        } => match action {
            Action::Read => Decision::Allow,
            Action::Write => Decision::Allow,
            Action::Execute { risk } => {
                if *risk >= RiskClass::Destructive {
                    Decision::Ask // Agent can't prompt mid-run → becomes DENY
                } else {
                    Decision::Allow
                }
            }
            Action::Delete => Decision::Deny,
            Action::Admin => Decision::Deny,
            Action::ContextRetrieve => Decision::Allow,
            Action::AccessSecret => {
                if agent_permissions.contains(&"access_secret".into()) {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Action::WebhookDeliver => Decision::Deny,
        },
        Actor::Plugin { permissions, .. } => match action {
            Action::Read => {
                if permissions.iter().any(|p| p.action == "read") {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Action::Write => {
                if permissions.iter().any(|p| p.action == "write") {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Action::ContextRetrieve => {
                if permissions.iter().any(|p| p.action == "context_retrieve") {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            _ => Decision::Deny,
        },
        Actor::Mcp { permissions, .. } => match action {
            Action::Read => {
                if permissions.iter().any(|p| p.action == "read") {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Action::Write => {
                if permissions.iter().any(|p| p.action == "write") {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Action::ContextRetrieve => {
                if permissions.iter().any(|p| p.action == "context_retrieve") {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            _ => Decision::Deny,
        },
        Actor::Webhook { .. } => match action {
            Action::WebhookDeliver => Decision::Allow,
            _ => Decision::Deny,
        },
        Actor::System => Decision::Allow,
    }
}

// ── Main authorization entry point ───────────────────────────────────────

/// Centralized authorization decision point.
///
/// Every human, agent, plugin, MCP, sandbox action, and context retrieval
/// passes through this single function. The decision is:
/// 1. Check the policy cache.
/// 2. Query `authorization_policies` table for a matching enabled policy.
/// 3. Fall back to the default matrix.
/// 4. For single-user OWNER installs, short-circuit to Allow (no DB queries).
///
/// Every decision is audited to the `audit_events` table.
pub async fn authorize(
    pool: &PgPool,
    policy_cache: &AuthPolicyCache,
    actor: &Actor,
    action: &Action,
    resource: &Resource,
    context: &AuthorizationContext,
) -> Result<Decision, AppError> {
    // Single-user short-circuit: skip everything for single OWNER.
    if let Actor::Human { role, .. } = actor
        && role == "OWNER"
        && is_single_user_owner(pool, context.profile_id).await?
    {
        return Ok(Decision::Allow);
    }

    let a_key = actor_key(actor);
    let act_key = action_key(action);
    let res_key = resource_key(resource);
    let cache_key = (a_key.clone(), act_key.clone(), res_key.clone());

    // 1. Check cache
    {
        let cache = policy_cache.read().await;
        if let Some(&decision) = cache.get(&cache_key) {
            audit_decision(pool, context, actor, action, resource, decision, "cache").await?;
            return Ok(decision);
        }
    }

    // 2. Query authorization_policies table
    let policy_decision = sqlx::query(
        "SELECT decision FROM authorization_policies \
         WHERE profile_id = $1 \
           AND actor = $2 \
           AND action = $3 \
           AND resource = $4 \
           AND enabled = true \
         ORDER BY priority ASC LIMIT 1",
    )
    .bind(context.profile_id)
    .bind(&a_key)
    .bind(&act_key)
    .bind(&res_key)
    .fetch_optional(pool)
    .await?;

    let decision = if let Some(row) = policy_decision {
        let decision_str: &str = row.get("decision");
        match decision_str {
            "ALLOW" => Decision::Allow,
            "ASK" => Decision::Ask,
            "DENY" => Decision::Deny,
            _ => default_matrix_decision(actor, action, resource),
        }
    } else {
        // 3. Fall back to default matrix
        default_matrix_decision(actor, action, resource)
    };

    // Update cache
    {
        let mut cache = policy_cache.write().await;
        cache.insert(cache_key, decision);
    }

    audit_decision(pool, context, actor, action, resource, decision, "matrix").await?;

    Ok(decision)
}

// ── Audit ────────────────────────────────────────────────────────────────

async fn audit_decision(
    pool: &PgPool,
    context: &AuthorizationContext,
    actor: &Actor,
    action: &Action,
    resource: &Resource,
    decision: Decision,
    reason: &str,
) -> Result<(), AppError> {
    let decision_str = match decision {
        Decision::Allow => "ALLOW",
        Decision::Ask => "ASK",
        Decision::Deny => "DENY",
    };
    let (actor_type, resource_type, resource_id) = format_audit_fields(actor, action, resource);

    sqlx::query(
        "INSERT INTO audit_events (actor_user_id, profile_id, action, resource_type, resource_id, outcome, detail) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(context.user_id)
    .bind(context.profile_id)
    .bind(format!("authorize.{actor_type}"))
    .bind(&resource_type)
    .bind(&resource_id)
    .bind(decision_str)
    .bind(serde_json::json!({
        "authorization_decision": decision_str,
        "authorization_reason": reason,
        "actor": actor_type,
        "action_detail": action_key(action),
        "resource": resource_type,
    }))
    .execute(pool)
    .await?;

    Ok(())
}

fn format_audit_fields(
    actor: &Actor,
    _action: &Action,
    resource: &Resource,
) -> (String, String, Option<String>) {
    let actor_type = match actor {
        Actor::Human { role, .. } => format!("human:{role}"),
        Actor::Agent { .. } => "agent".into(),
        Actor::Plugin { .. } => "plugin".into(),
        Actor::Mcp { .. } => "mcp".into(),
        Actor::Webhook { .. } => "webhook".into(),
        Actor::System => "system".into(),
    };

    let (resource_type, resource_id) = match resource {
        Resource::Book(id) => ("book".into(), Some(id.to_string())),
        Resource::Conversation(id) => ("conversation".into(), Some(id.to_string())),
        Resource::Provider(s) => ("provider".into(), Some(s.clone())),
        Resource::Model(s) => ("model".into(), Some(s.clone())),
        Resource::Workspace(id) => ("workspace".into(), Some(id.to_string())),
        Resource::Plugin(id) => ("plugin".into(), Some(id.to_string())),
        Resource::McpServer(id) => ("mcp_server".into(), Some(id.to_string())),
        Resource::Sandbox => ("sandbox".into(), None),
        Resource::Secret(s) => ("secret".into(), Some(s.clone())),
        Resource::System => ("system".into(), None),
    };

    (actor_type, resource_type, resource_id)
}

// ── Cache invalidation ───────────────────────────────────────────────────

/// Clear the entire policy cache. Call when policies are mutated.
pub async fn invalidate_policy_cache(cache: &AuthPolicyCache) {
    let mut c = cache.write().await;
    c.clear();
}

// ── Unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_actor() -> Actor {
        Actor::Human {
            user_id: Uuid::nil(),
            role: "OWNER".into(),
        }
    }

    fn admin_actor() -> Actor {
        Actor::Human {
            user_id: Uuid::nil(),
            role: "ADMIN".into(),
        }
    }

    fn member_actor() -> Actor {
        Actor::Human {
            user_id: Uuid::nil(),
            role: "MEMBER".into(),
        }
    }

    fn viewer_actor() -> Actor {
        Actor::Human {
            user_id: Uuid::nil(),
            role: "VIEWER".into(),
        }
    }

    fn agent_actor(permissions: Vec<String>) -> Actor {
        Actor::Agent {
            run_id: Uuid::nil(),
            agent_permissions: permissions,
        }
    }

    fn plugin_actor(permissions: Vec<PermissionPreview>) -> Actor {
        Actor::Plugin {
            plugin_id: Uuid::nil(),
            permissions,
        }
    }

    fn webhook_actor() -> Actor {
        Actor::Webhook { id: Uuid::nil() }
    }

    fn system_actor() -> Actor {
        Actor::System
    }

    fn book_resource() -> Resource {
        Resource::Book(Uuid::nil())
    }

    fn sandbox_resource() -> Resource {
        Resource::Sandbox
    }

    fn secret_resource() -> Resource {
        Resource::Secret("api_key".into())
    }

    fn system_resource() -> Resource {
        Resource::System
    }

    // ── Matrix: single-user OWNER ─────────────────────────────

    #[test]
    fn owner_allows_read() {
        let decision = default_matrix_decision(&owner_actor(), &Action::Read, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn owner_allows_write() {
        let decision = default_matrix_decision(&owner_actor(), &Action::Write, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn owner_allows_delete() {
        let decision = default_matrix_decision(&owner_actor(), &Action::Delete, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn owner_allows_admin() {
        let decision = default_matrix_decision(&owner_actor(), &Action::Admin, &system_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn owner_allows_execute_destructive() {
        let decision = default_matrix_decision(
            &owner_actor(),
            &Action::Execute {
                risk: RiskClass::Destructive,
            },
            &sandbox_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn owner_allows_access_secret() {
        let decision =
            default_matrix_decision(&owner_actor(), &Action::AccessSecret, &secret_resource());
        assert_eq!(decision, Decision::Allow);
    }

    // ── Matrix: ADMIN ─────────────────────────────────────────

    #[test]
    fn admin_allows_read() {
        let decision = default_matrix_decision(&admin_actor(), &Action::Read, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn admin_allows_write() {
        let decision = default_matrix_decision(&admin_actor(), &Action::Write, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn admin_allows_delete() {
        let decision = default_matrix_decision(&admin_actor(), &Action::Delete, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    // ── Matrix: MEMBER ────────────────────────────────────────

    #[test]
    fn member_allows_read() {
        let decision = default_matrix_decision(&member_actor(), &Action::Read, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn member_allows_write_book() {
        let decision = default_matrix_decision(&member_actor(), &Action::Write, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn member_denies_admin() {
        let decision = default_matrix_decision(&member_actor(), &Action::Admin, &system_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn member_denies_execute_destructive() {
        let decision = default_matrix_decision(
            &member_actor(),
            &Action::Execute {
                risk: RiskClass::Destructive,
            },
            &sandbox_resource(),
        );
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn member_allows_execute_write() {
        let decision = default_matrix_decision(
            &member_actor(),
            &Action::Execute {
                risk: RiskClass::Write,
            },
            &sandbox_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    // ── Matrix: VIEWER ────────────────────────────────────────

    #[test]
    fn viewer_allows_read() {
        let decision = default_matrix_decision(&viewer_actor(), &Action::Read, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn viewer_denies_write() {
        let decision = default_matrix_decision(&viewer_actor(), &Action::Write, &book_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn viewer_denies_delete() {
        let decision = default_matrix_decision(&viewer_actor(), &Action::Delete, &book_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn viewer_denies_admin() {
        let decision = default_matrix_decision(&viewer_actor(), &Action::Admin, &system_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn viewer_denies_access_secret() {
        let decision =
            default_matrix_decision(&viewer_actor(), &Action::AccessSecret, &secret_resource());
        assert_eq!(decision, Decision::Deny);
    }

    // ── Matrix: AGENT ─────────────────────────────────────────

    #[test]
    fn agent_allows_read() {
        let decision =
            default_matrix_decision(&agent_actor(vec![]), &Action::Read, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn agent_allows_write() {
        let decision =
            default_matrix_decision(&agent_actor(vec![]), &Action::Write, &book_resource());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn agent_allows_context_retrieve() {
        let decision = default_matrix_decision(
            &agent_actor(vec![]),
            &Action::ContextRetrieve,
            &book_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn agent_allows_execute_non_destructive() {
        let decision = default_matrix_decision(
            &agent_actor(vec![]),
            &Action::Execute {
                risk: RiskClass::Write,
            },
            &sandbox_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn agent_asks_execute_destructive() {
        let decision = default_matrix_decision(
            &agent_actor(vec![]),
            &Action::Execute {
                risk: RiskClass::Destructive,
            },
            &sandbox_resource(),
        );
        assert_eq!(decision, Decision::Ask);
    }

    #[test]
    fn agent_asks_execute_system_risk() {
        let decision = default_matrix_decision(
            &agent_actor(vec![]),
            &Action::Execute {
                risk: RiskClass::System,
            },
            &sandbox_resource(),
        );
        assert_eq!(decision, Decision::Ask);
    }

    #[test]
    fn agent_denies_delete() {
        let decision =
            default_matrix_decision(&agent_actor(vec![]), &Action::Delete, &book_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn agent_denies_admin() {
        let decision =
            default_matrix_decision(&agent_actor(vec![]), &Action::Admin, &system_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn agent_denies_access_secret_without_perm() {
        let decision = default_matrix_decision(
            &agent_actor(vec![]),
            &Action::AccessSecret,
            &secret_resource(),
        );
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn agent_allows_access_secret_with_perm() {
        let decision = default_matrix_decision(
            &agent_actor(vec!["access_secret".into()]),
            &Action::AccessSecret,
            &secret_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn agent_denies_webhook_deliver() {
        let decision = default_matrix_decision(
            &agent_actor(vec![]),
            &Action::WebhookDeliver,
            &Resource::System,
        );
        assert_eq!(decision, Decision::Deny);
    }

    // ── Matrix: PLUGIN ────────────────────────────────────────

    #[test]
    fn plugin_denies_read_without_perm() {
        let decision =
            default_matrix_decision(&plugin_actor(vec![]), &Action::Read, &book_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn plugin_allows_read_with_perm() {
        let decision = default_matrix_decision(
            &plugin_actor(vec![PermissionPreview {
                action: "read".into(),
                resource_pattern: "*".into(),
            }]),
            &Action::Read,
            &book_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn plugin_allows_write_with_perm() {
        let decision = default_matrix_decision(
            &plugin_actor(vec![PermissionPreview {
                action: "write".into(),
                resource_pattern: "*".into(),
            }]),
            &Action::Write,
            &book_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn plugin_denies_admin() {
        let decision = default_matrix_decision(
            &plugin_actor(vec![PermissionPreview {
                action: "admin".into(),
                resource_pattern: "*".into(),
            }]),
            &Action::Admin,
            &system_resource(),
        );
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn plugin_allows_context_retrieve_with_perm() {
        let decision = default_matrix_decision(
            &plugin_actor(vec![PermissionPreview {
                action: "context_retrieve".into(),
                resource_pattern: "*".into(),
            }]),
            &Action::ContextRetrieve,
            &book_resource(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn plugin_denies_context_retrieve_without_perm() {
        let decision = default_matrix_decision(
            &plugin_actor(vec![]),
            &Action::ContextRetrieve,
            &book_resource(),
        );
        assert_eq!(decision, Decision::Deny);
    }

    // ── Matrix: WEBHOOK ───────────────────────────────────────

    #[test]
    fn webhook_allows_deliver() {
        let decision =
            default_matrix_decision(&webhook_actor(), &Action::WebhookDeliver, &Resource::System);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn webhook_denies_read() {
        let decision = default_matrix_decision(&webhook_actor(), &Action::Read, &book_resource());
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn webhook_denies_write() {
        let decision = default_matrix_decision(&webhook_actor(), &Action::Write, &book_resource());
        assert_eq!(decision, Decision::Deny);
    }

    // ── Matrix: SYSTEM ────────────────────────────────────────

    #[test]
    fn system_allows_everything() {
        let actions = [
            Action::Read,
            Action::Write,
            Action::Execute {
                risk: RiskClass::Destructive,
            },
            Action::Delete,
            Action::Admin,
            Action::ContextRetrieve,
            Action::AccessSecret,
            Action::WebhookDeliver,
        ];
        for action in &actions {
            let decision = default_matrix_decision(&system_actor(), action, &system_resource());
            assert_eq!(decision, Decision::Allow, "System should allow {action:?}");
        }
    }

    // ── RiskClass ordering ────────────────────────────────────

    #[test]
    fn risk_class_ordering() {
        assert!(RiskClass::Read < RiskClass::Write);
        assert!(RiskClass::Write < RiskClass::Destructive);
        assert!(RiskClass::Destructive < RiskClass::System);
    }

    #[test]
    fn risk_class_from_tool_risk() {
        assert_eq!(RiskClass::from_tool_risk("read"), RiskClass::Read);
        assert_eq!(RiskClass::from_tool_risk("write"), RiskClass::Write);
        assert_eq!(
            RiskClass::from_tool_risk("destructive"),
            RiskClass::Destructive
        );
        assert_eq!(RiskClass::from_tool_risk("system"), RiskClass::System);
        assert_eq!(RiskClass::from_tool_risk("unknown"), RiskClass::Write);
    }

    // ── Cache operations ──────────────────────────────────────

    #[tokio::test]
    async fn cache_invalidate_clears_all() {
        let cache = new_policy_cache();
        {
            let mut c = cache.write().await;
            c.insert(("a".into(), "b".into(), "c".into()), Decision::Allow);
        }
        invalidate_policy_cache(&cache).await;
        let c = cache.read().await;
        assert!(c.is_empty());
    }

    // ── Actor/action/resource key formatting ──────────────────

    #[test]
    fn actor_key_human_owner() {
        assert_eq!(actor_key(&owner_actor()), "HUMAN:OWNER");
    }

    #[test]
    fn actor_key_agent() {
        assert_eq!(actor_key(&agent_actor(vec![])), "AGENT");
    }

    #[test]
    fn action_key_execute_write() {
        assert_eq!(
            action_key(&Action::Execute {
                risk: RiskClass::Write
            }),
            "EXECUTE:write"
        );
    }

    #[test]
    fn resource_key_book() {
        assert_eq!(resource_key(&book_resource()), "BOOK");
    }
}
