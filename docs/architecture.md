# Architecture

See [`architecture-plan.md`](architecture-plan.md) and the decisions in [`adr/`](adr/). Gobrowse OS uses an event-aware modular monolith, typed ports for external systems, PostgreSQL as the durable authority, and separately deployed services only at privilege boundaries.
