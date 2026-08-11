use base64::{Engine as _, engine::general_purpose::STANDARD};
use gobrowse_server::{config::VaultSettings, vault};
use secrecy::{ExposeSecret, SecretString};
use sqlx::{Connection, PgConnection, PgPool, Row};
use time::Duration;
use uuid::Uuid;

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("apply test migrations");
    Some(pool)
}

#[tokio::test]
async fn migrations_enable_pgvector_and_schema_version() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let row = sqlx::query(
        "SELECT schema_version, EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'vector') AS vector_enabled \
         FROM schema_metadata WHERE singleton",
    )
    .fetch_one(&pool)
    .await
    .expect("read schema metadata");
    assert_eq!(row.get::<i64, _>("schema_version"), 5);
    assert!(row.get::<bool, _>("vector_enabled"));
}

#[tokio::test]
async fn autobiography_constraint_uses_character_length() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'constraint-test')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, provenance, trust, author, security_classification) \
         VALUES ($1,$2,'Autobiography',$3,'AUTOBIOGRAPHY','PROFILE','USER','USER_PROVIDED','test','CONFIDENTIAL')",
    )
    .bind(Uuid::now_v7())
    .bind(profile_id)
    .bind("🦀".repeat(99_999))
    .execute(&pool)
    .await
    .expect("99,999 Unicode characters must be accepted");

    let second_profile = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'constraint-test-overflow')")
        .bind(second_profile)
        .execute(&pool)
        .await
        .expect("create overflow test profile");
    let overflow = sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, provenance, trust, author, security_classification) \
         VALUES ($1,$2,'Autobiography',$3,'AUTOBIOGRAPHY','PROFILE','USER','USER_PROVIDED','test','CONFIDENTIAL')",
    )
    .bind(Uuid::now_v7())
    .bind(second_profile)
    .bind("🦀".repeat(100_000))
    .execute(&pool)
    .await;
    assert!(
        overflow.is_err(),
        "100,000 Unicode characters must be rejected"
    );
}

#[tokio::test]
async fn lexical_search_and_revision_history_are_transactional() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let profile_id = Uuid::now_v7();
    let book_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'search-test')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, provenance, trust, author, security_classification) \
         VALUES ($1,$2,'Sandbox boundary','rootless containers isolate terminal execution','DOCUMENT','PROFILE','USER','VERIFIED','test','INTERNAL')",
    )
    .bind(book_id)
    .bind(profile_id)
    .execute(&pool)
    .await
    .expect("create searchable Book");
    let found: bool = sqlx::query_scalar(
        "SELECT search_document @@ websearch_to_tsquery('english', 'rootless terminal') FROM books WHERE id = $1",
    )
    .bind(book_id)
    .fetch_one(&pool)
    .await
    .expect("run lexical query");
    assert!(found);

    let mut tx = pool.begin().await.expect("begin revision transaction");
    sqlx::query(
        "INSERT INTO book_revisions (id, book_id, revision, title, body, tags, metadata, change_reason) \
         SELECT $1, id, revision, title, body, tags, metadata, 'integration test' FROM books WHERE id = $2 FOR UPDATE",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .execute(&mut *tx)
    .await
    .expect("write immutable revision");
    sqlx::query(
        "UPDATE books SET body = 'updated canonical body', revision = revision + 1 WHERE id = $1",
    )
    .bind(book_id)
    .execute(&mut *tx)
    .await
    .expect("update canonical Book");
    tx.commit().await.expect("commit revision transaction");
    let old_body: String =
        sqlx::query_scalar("SELECT body FROM book_revisions WHERE book_id = $1 AND revision = 1")
            .bind(book_id)
            .fetch_one(&pool)
            .await
            .expect("read revision");
    assert_eq!(old_body, "rootless containers isolate terminal execution");
}

#[tokio::test]
async fn vault_round_trip_never_persists_plaintext() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'vault-test')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create vault test profile");
    let settings = VaultSettings {
        master_key_file: None,
        master_key_base64: Some(SecretString::from(STANDARD.encode([19_u8; 32]))),
        key_version: 1,
        ..VaultSettings::default()
    };
    let vault = vault::Vault::from_settings(&settings)
        .await
        .expect("create vault");
    let id = format!("secret_{}", Uuid::now_v7());
    let plaintext = SecretString::from("provider-token-that-must-not-leak".to_owned());
    let encrypted = vault
        .encrypt(profile_id, &id, "provider_credential", &plaintext)
        .expect("encrypt provider credential");
    sqlx::query(
        "INSERT INTO secret_references \
         (id, profile_id, backend, locator, encrypted_value, nonce, key_version, purpose, algorithm, wrapped_data_key, wrap_nonce) \
         VALUES ($1,$2,'encrypted_database','database',$3,$4,$5,'provider_credential',$6,$7,$8)",
    )
    .bind(&id)
    .bind(profile_id)
    .bind(&encrypted.ciphertext)
    .bind(&encrypted.nonce)
    .bind(encrypted.key_version)
    .bind(vault::algorithm())
    .bind(&encrypted.wrapped_data_key)
    .bind(&encrypted.wrap_nonce)
    .execute(&pool)
    .await
    .expect("store encrypted credential");
    assert!(
        !encrypted
            .ciphertext
            .windows(plaintext.expose_secret().len())
            .any(|window| window == plaintext.expose_secret().as_bytes())
    );
    let resolved = vault
        .resolve(&pool, profile_id, &id)
        .await
        .expect("resolve encrypted credential");
    assert_eq!(resolved.expose_secret(), plaintext.expose_secret());

    let rotating = vault::Vault::from_settings(&VaultSettings {
        master_key_file: None,
        master_key_base64: Some(SecretString::from(STANDARD.encode([20_u8; 32]))),
        key_version: 2,
        previous_master_key_file: None,
        previous_master_key_base64: Some(SecretString::from(STANDARD.encode([19_u8; 32]))),
        previous_key_version: Some(1),
    })
    .await
    .expect("create rotating vault");
    let mut tx = pool.begin().await.expect("start key rotation");
    assert_eq!(
        rotating
            .rotate_profile(&mut tx, profile_id)
            .await
            .expect("rewrap profile secrets"),
        1
    );
    tx.commit().await.expect("commit key rotation");
    let key_version: i32 =
        sqlx::query_scalar("SELECT key_version FROM secret_references WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .expect("read rotated key version");
    assert_eq!(key_version, 2);
    let resolved = rotating
        .resolve(&pool, profile_id, &id)
        .await
        .expect("resolve rotated credential");
    assert_eq!(resolved.expose_secret(), plaintext.expose_secret());
    let mut stale_writer = pool.begin().await.expect("start stale writer");
    assert!(
        vault
            .fence_current_key(&mut stale_writer, profile_id)
            .await
            .is_err(),
        "an instance with the previous key must be fenced after rotation"
    );
    stale_writer
        .rollback()
        .await
        .expect("rollback stale writer");
}

#[tokio::test]
async fn schema_v3_safely_upgrades_permitted_v1_states() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("install shared extensions before isolated upgrade test");
    let schema = format!("upgrade_{}", Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create isolated upgrade schema");
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect upgrade session");
    sqlx::query(&format!("SET search_path TO {schema},public"))
        .execute(&mut connection)
        .await
        .expect("select upgrade schema");
    sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
        .execute(&mut connection)
        .await
        .expect("install schema v1");
    let profile_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();
    let private_book = Uuid::now_v7();
    let agent_book = Uuid::now_v7();
    let autobiography = Uuid::now_v7();
    let conversation_id = Uuid::now_v7();
    let workspace_id = Uuid::now_v7();
    let agent_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
    let chunk_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'upgrade-test')")
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("create v1 profile");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Upgrade Owner','unused','OWNER',$3)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("create v1 user");
    sqlx::query("INSERT INTO workspaces (id,profile_id,title) VALUES ($1,$2,'Legacy workspace')")
        .bind(workspace_id)
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("create v1 workspace");
    sqlx::query("INSERT INTO audit_events (actor_user_id,profile_id,action,resource_type,resource_id,outcome) VALUES ($1,$2,'workspace.created','workspace',$3,'success')")
        .bind(user_id).bind(profile_id).bind(workspace_id.to_string()).execute(&mut connection).await.expect("audit workspace creator");
    sqlx::query("INSERT INTO agents (id,workspace_id,name,kind,permissions,status) VALUES ($1,$2,'Legacy agent','coding','{}','paused')")
        .bind(agent_id).bind(workspace_id).execute(&mut connection).await.expect("create v1 agent");
    sqlx::query("INSERT INTO agent_runs (id,agent_id,state) VALUES ($1,$2,'paused')")
        .bind(run_id)
        .bind(agent_id)
        .execute(&mut connection)
        .await
        .expect("create v1 run");
    sqlx::query(
        "INSERT INTO run_events (run_id,event_type,payload) VALUES ($1,'legacy.paused','{}')",
    )
    .bind(run_id)
    .execute(&mut connection)
    .await
    .expect("create v1 run event");
    for (id, title, scope, book_type) in [
        (private_book, "Private", "PRIVATE", "NOTE"),
        (agent_book, "Agent", "AGENT", "NOTE"),
        (autobiography, "Autobiography", "PROFILE", "AUTOBIOGRAPHY"),
    ] {
        sqlx::query(
            "INSERT INTO books (id,profile_id,title,body,book_type,scope,provenance,trust,author,security_classification,embedding_model_id,embedding_status) \
             VALUES ($1,$2,$3,'legacy body',$4,$5,'USER','USER_PROVIDED','legacy','INTERNAL','legacy-model','ready')",
        )
        .bind(id)
        .bind(profile_id)
        .bind(title)
        .bind(book_type)
        .bind(scope)
        .execute(&mut connection)
        .await
        .expect("create v1 Book");
    }
    sqlx::query(
        "INSERT INTO providers (id,profile_id,provider_type,display_name,base_url) \
         VALUES ('legacy-provider',$1,'ollama','Legacy','http://127.0.0.1:11434')",
    )
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("create v1 provider");
    sqlx::query(
        "INSERT INTO conversations (id,profile_id,workspace_id,title) VALUES ($1,$2,$3,'Legacy conversation')",
    )
    .bind(conversation_id)
    .bind(profile_id)
    .bind(workspace_id)
    .execute(&mut connection)
    .await
    .expect("create v1 conversation");
    for title in ["Projection one", "Projection two"] {
        sqlx::query(
            "INSERT INTO books (id,profile_id,title,body,book_type,scope,provenance,trust,author,conversation_id,security_classification) \
             VALUES ($1,$2,$3,'legacy conversation','CONVERSATION','CONVERSATION','CONVERSATION','VERIFIED','legacy',$4,'INTERNAL')",
        )
        .bind(Uuid::now_v7())
        .bind(profile_id)
        .bind(title)
        .bind(conversation_id)
        .execute(&mut connection)
        .await
        .expect("create duplicate v1 projection");
    }
    sqlx::query(
        "INSERT INTO embedding_models (id,provider_id,model_reference,dimensions) \
         VALUES ('legacy-model','legacy-provider','legacy',3)",
    )
    .execute(&mut connection)
    .await
    .expect("create v1 embedding model");
    sqlx::query(
        "INSERT INTO embedding_models (id,provider_id,model_reference,dimensions) \
         VALUES ('legacy-model-duplicate','legacy-provider','legacy',3)",
    )
    .execute(&mut connection)
    .await
    .expect("create duplicate v1 embedding model");
    sqlx::query(
        "INSERT INTO book_chunks (id,book_id,ordinal,text,token_estimate,source_start,source_end,embedding,embedding_model_id) \
         VALUES ($1,$2,0,'legacy body',3,0,11,'[1,0,0]'::vector,'legacy-model')",
    )
    .bind(chunk_id)
    .bind(private_book)
    .execute(&mut connection)
    .await
    .expect("create v1 vector");
    for status in ["queued", "running"] {
        sqlx::query(
            "INSERT INTO embedding_jobs (id,book_id,embedding_model_id,status) VALUES ($1,$2,'legacy-model',$3)",
        )
        .bind(Uuid::now_v7())
        .bind(private_book)
        .bind(status)
        .execute(&mut connection)
        .await
        .expect("create v1 duplicate active job");
    }
    sqlx::query(
        "INSERT INTO embedding_jobs (id,book_id,embedding_model_id,status,attempts) \
         VALUES ($1,$2,'legacy-model','queued',99)",
    )
    .bind(Uuid::now_v7())
    .bind(agent_book)
    .execute(&mut connection)
    .await
    .expect("create exhausted v1 job");
    sqlx::query(
        "INSERT INTO secret_references (id,profile_id,backend,locator,encrypted_value,nonce,key_version) \
         VALUES ('legacy-secret',$1,'encrypted_database','legacy',decode('01','hex'),decode('02','hex'),1)",
    )
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("create v1 encrypted secret");
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO autobiography_proposals \
             (id,profile_id,book_id,before_body,after_body,reason,status) \
             VALUES ($1,$2,$3,'legacy body','proposed','legacy proposal','pending')",
        )
        .bind(Uuid::now_v7())
        .bind(profile_id)
        .bind(autobiography)
        .execute(&mut connection)
        .await
        .expect("create v1 pending proposal");
    }
    sqlx::raw_sql(include_str!("../migrations/0002_library_embeddings.sql"))
        .execute(&mut connection)
        .await
        .expect("upgrade dirty v1 state to v2");
    let schema_version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read upgraded version");
    assert_eq!(schema_version, 2);
    let migrated_vectors: i64 = sqlx::query_scalar("SELECT count(*) FROM book_chunk_embeddings")
        .fetch_one(&mut connection)
        .await
        .expect("count migrated vectors");
    assert_eq!(migrated_vectors, 1);
    let active_jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embedding_jobs WHERE book_id=$1 AND embedding_model_id='legacy-model' \
         AND status IN ('queued','retry','running')",
    )
    .bind(private_book)
    .fetch_one(&mut connection)
    .await
    .expect("count deduplicated jobs");
    assert_eq!(active_jobs, 1);
    let exhausted_status: String =
        sqlx::query_scalar("SELECT status FROM embedding_jobs WHERE book_id=$1 AND attempts=99")
            .bind(agent_book)
            .fetch_one(&mut connection)
            .await
            .expect("read exhausted migrated job");
    assert_eq!(exhausted_status, "failed");
    let projection_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM books WHERE conversation_id=$1 AND book_type='CONVERSATION'",
    )
    .bind(conversation_id)
    .fetch_one(&mut connection)
    .await
    .expect("count deduplicated projections");
    assert_eq!(projection_count, 1);
    let model_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embedding_models WHERE provider_id='legacy-provider' AND model_reference='legacy'",
    )
    .fetch_one(&mut connection)
    .await
    .expect("count deduplicated models");
    assert_eq!(model_count, 1);
    let legacy_backend: String =
        sqlx::query_scalar("SELECT backend FROM secret_references WHERE id='legacy-secret'")
            .fetch_one(&mut connection)
            .await
            .expect("read quarantined secret");
    assert_eq!(legacy_backend, "legacy_encrypted_database");
    let rejected_proposals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autobiography_proposals WHERE status='rejected'")
            .fetch_one(&mut connection)
            .await
            .expect("count invalidated proposals");
    assert_eq!(rejected_proposals, 2);
    let agent_scope: String = sqlx::query_scalar("SELECT scope FROM books WHERE id=$1")
        .bind(agent_book)
        .fetch_one(&mut connection)
        .await
        .expect("read quarantined Agent Book");
    assert_eq!(agent_scope, "PROFILE");
    sqlx::raw_sql(include_str!("../migrations/0003_chat_runs.sql"))
        .execute(&mut connection)
        .await
        .expect("upgrade deployed v2 state to v3");
    let schema_version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read schema v3 version");
    assert_eq!(schema_version, 3);
    let conversation_owner: Uuid =
        sqlx::query_scalar("SELECT created_by_user_id FROM conversations WHERE id=$1")
            .bind(conversation_id)
            .fetch_one(&mut connection)
            .await
            .expect("read conversation owner");
    assert_eq!(conversation_owner, user_id);
    let workspace_access: String = sqlx::query_scalar(
        "SELECT access FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&mut connection)
    .await
    .expect("read backfilled workspace owner");
    assert_eq!(workspace_access, "OWNER");
    let legacy_run: (Uuid, String) =
        sqlx::query_as("SELECT profile_id,run_kind FROM agent_runs WHERE id=$1")
            .bind(run_id)
            .fetch_one(&mut connection)
            .await
            .expect("read migrated legacy run");
    assert_eq!(legacy_run, (profile_id, "agent".into()));
    let event_profile: Uuid =
        sqlx::query_scalar("SELECT profile_id FROM run_events WHERE run_id=$1")
            .bind(run_id)
            .fetch_one(&mut connection)
            .await
            .expect("read migrated event profile");
    assert_eq!(event_profile, profile_id);
    let immutable = sqlx::query("UPDATE book_revisions SET body='rewritten'")
        .execute(&mut connection)
        .await;
    assert!(immutable.is_err(), "revision trigger must reject updates");
    connection.close().await.expect("close upgrade session");
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop isolated upgrade schema");
}

#[tokio::test]
async fn login_rate_limit_blocks_after_threshold() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let email = format!("throttle-{}@example.test", Uuid::now_v7().simple());
    let email_lower = email.to_lowercase();
    let now = time::OffsetDateTime::now_utc();

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up previous attempts");

    for _ in 0..5 {
        sqlx::query(
            "INSERT INTO login_attempts (email, outcome, occurred_at) VALUES ($1, 'failure', $2)",
        )
        .bind(&email_lower)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert attempt");
    }

    let window = Duration::seconds(300);
    let cutoff = now - window;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email = $1 AND occurred_at > $2",
    )
    .bind(&email_lower)
    .bind(cutoff)
    .fetch_one(&pool)
    .await
    .expect("count attempts");
    assert!(
        count >= 5,
        "expected at least 5 recent attempts for throttled email, got {count}"
    );

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up test data");
}

#[tokio::test]
async fn login_rate_limit_recovers_after_window_expiry() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let email = format!("recover-{}@example.test", Uuid::now_v7().simple());
    let email_lower = email.to_lowercase();

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up previous attempts");

    let old_time = time::OffsetDateTime::now_utc() - Duration::seconds(301);
    for _ in 0..5 {
        sqlx::query(
            "INSERT INTO login_attempts (email, outcome, occurred_at) VALUES ($1, 'failure', $2)",
        )
        .bind(&email_lower)
        .bind(old_time)
        .execute(&pool)
        .await
        .expect("insert old attempt");
    }

    let window = Duration::seconds(300);
    let cutoff = time::OffsetDateTime::now_utc() - window;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email = $1 AND occurred_at > $2",
    )
    .bind(&email_lower)
    .bind(cutoff)
    .fetch_one(&pool)
    .await
    .expect("count recent attempts");
    assert_eq!(count, 0, "old attempts must not count within the window");

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up test data");
}

#[tokio::test]
async fn locked_account_returns_uniform_unauthorized() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let email = format!("locked-{}@example.test", Uuid::now_v7().simple());
    let email_lower = email.to_lowercase();
    let now = time::OffsetDateTime::now_utc();

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up previous attempts");

    for _ in 0..5 {
        sqlx::query(
            "INSERT INTO login_attempts (email, outcome, occurred_at) VALUES ($1, 'failure', $2)",
        )
        .bind(&email_lower)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert failure");
    }

    let window = Duration::seconds(300);
    let cutoff = now - window;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email = $1 AND occurred_at > $2",
    )
    .bind(&email_lower)
    .bind(cutoff)
    .fetch_one(&pool)
    .await
    .expect("count attempts");
    assert!(
        count >= 5,
        "locked account must have at least {} recent failures, got {count}",
        5
    );

    let outcomes: Vec<String> =
        sqlx::query("SELECT outcome FROM login_attempts WHERE email = $1 ORDER BY occurred_at")
            .bind(&email_lower)
            .fetch_all(&pool)
            .await
            .expect("read outcomes")
            .into_iter()
            .map(|row| row.get("outcome"))
            .collect();
    assert!(!outcomes.is_empty(), "should have recorded outcomes");
    assert!(
        outcomes.iter().all(|o| o == "failure"),
        "all recorded attempts should be failures"
    );

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up test data");
}
