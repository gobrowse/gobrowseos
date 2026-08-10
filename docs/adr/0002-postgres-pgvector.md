# ADR 0002: PostgreSQL and pgvector

Status: Accepted

Use PostgreSQL for relational state, full-text search, event durability, jobs, and pgvector embeddings. Rank fusion happens in the application over independently retrieved lexical and semantic candidates. This avoids Redis, Elasticsearch, and a separate vector service in the minimum deployment. Canonical Book bodies are never replaced by chunks or vectors.
