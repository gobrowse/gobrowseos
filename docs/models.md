# Models and Providers

Every runtime model is identified by `provider + model`. Administrators configure endpoints and credential references in data, not code. Capabilities are explicit and checked before use. Provider adapters translate the neutral message model at the boundary and never forward another provider's private reasoning signatures.

Fallback is permitted for classified availability, rate-limit, credentials, capability, timeout, and context errors. Tool side effects are not retried with a fallback model. Embedding and reranking providers use separate traits and configuration.

Embedding adapters support OpenAI-compatible `/embeddings` APIs and Ollama `/api/embed`. Remote providers require HTTPS. Local Ollama requires `features.local_embeddings = true` and cannot receive a credential. Provider credentials are vault references restricted to declared hosts; endpoints are DNS-resolved and pinned after prohibited-address checks.
