# ADR 0004: Provider-Neutral Capabilities

Status: Accepted

Chat, embedding, reranking, image, and speech providers are separate traits. Runtime model identity is `provider + model`; capabilities are data and checked before selection. Neutral message history excludes provider-private signatures so switching models cannot forward incompatible hidden state. HTTP adapters use shared hardened clients rather than vendor SDK types in the core.
