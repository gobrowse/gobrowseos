-- 0005_webhooks.sql
-- Enable inbound webhook delivery verification with raw HMAC secrets.
-- The secret_reference column (vault) is made nullable so that inbound
-- webhooks can store a raw bytea secret_key without requiring vault setup.
-- Scope cut: vault integration for webhook secrets is deferred.

ALTER TABLE webhooks ALTER COLUMN secret_reference DROP NOT NULL;
ALTER TABLE webhooks ADD COLUMN secret_key bytea;

UPDATE schema_metadata SET schema_version = 5, updated_at = now() WHERE singleton;
