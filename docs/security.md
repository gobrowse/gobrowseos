# Security

See [`threat-model.md`](threat-model.md). Production requires TLS, an externally supplied master key, exact public-origin configuration, private database networking, and the optional rootless sandbox service for terminal execution. Never expose the app or database with default example credentials.

## Supply-Chain Exceptions

`deny.toml` narrowly ignores `RUSTSEC-2024-0436` (`paste`) and `RUSTSEC-2026-0173` (`proc-macro-error2`). They are maintenance-status advisories in Leptos 0.8 transitive dependencies, not known exploit advisories, and currently have no compatible safe upgrade. CI still runs both cargo-deny and cargo-audit; remove the exceptions as soon as Leptos removes those dependencies.

`cargo-audit` additionally ignores `RUSTSEC-2023-0071` for `rsa`. SQLx lists its optional MySQL implementation in lockfile metadata, but Gobrowse builds SQLx with default features disabled and PostgreSQL only; `cargo tree --target all -i rsa` confirms that RSA is absent from every selected target graph. Cargo-deny performs graph-aware advisory checks. This exception must be removed if any MySQL feature is introduced.
