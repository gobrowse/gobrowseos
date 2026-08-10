# ADR 0009: Credential Envelopes and Provider Network Policy

Status: Accepted

Encrypt each database credential with a random data-encryption key using AES-256-GCM. Wrap that key with an externally supplied, versioned 256-bit master key and bind both layers to profile, secret ID, purpose, and key version through authenticated associated data. APIs return metadata only. Rotation temporarily configures the current and immediately previous key and transactionally rewraps credentials.

Credentials declare allowed provider hosts. Authenticated providers require HTTPS. DNS is resolved, classified, and pinned before requests; metadata, link-local, unspecified, multicast, documentation, carrier-grade NAT, and benchmarking ranges are prohibited. Private endpoints are limited to credential-free Ollama when local embeddings are explicitly enabled.
