# Security

See [`threat-model.md`](threat-model.md). Production requires TLS, an externally supplied master key, exact public-origin configuration, private database networking, and the optional rootless sandbox service for terminal execution. Never expose the app or database with default example credentials.
