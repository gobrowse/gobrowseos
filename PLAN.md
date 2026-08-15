# PLAN.md — M4 Rootless Podman `keep-id` Runtime Proof

## Status, Decision, and Preserved M7 External Record

**M4 is `BLOCKED_EXTERNAL`.** The sandbox remains release-gated and off. This
plan records no successful runtime proof and authorizes no source or CI change
until the approved execution prerequisite below is available.

The caller-provided-pipe bounded stdio JSON-RPC framing slice is accepted at
`4c2bff3796ec7ac15b86a167d49828a1eb556b98`. Exact-SHA CI run
`31908353273` passed Rust `95069542559`, web `95069542603`,
supply-chain `95069542548`, and container `95069542574`. This accepts only
bounded stdio framing over caller-provided Tokio async pipes and its tests; it
does not create a process, establish a session, or prove real MCP
interoperability. **M7 remains incomplete.**

The sole remaining M7 gate is `BLOCKED_EXTERNAL`. It requires an approved,
immutable/pinned, independently implemented MCP stdio peer with recorded and
verified provenance, version, and content hash. Non-ignored CI must run a
real-peer integration test that performs `server/discover` and a
capability-authorized `tools/list` against that peer. A fixture, mock,
loopback peer, or in-repository substitute does not satisfy this requirement.

That independent pinned MCP peer prerequisite remains blocked; this M4 plan
does not alter, replace, or satisfy it.

## External Prerequisite

The smallest honest M4 proof requires all of the following, none of which is
available in the current environment:

1. an approved ephemeral **local** runner executing as a non-root user with a
   real rootless Podman installation;
2. preconfigured `/etc/subuid` and `/etc/subgid` entries for that non-root
   runner user;
3. the exact immutable image reference preloaded locally by digest; and
4. a trusted, required, non-ignored CI job bound to that runner.

The runner and its Podman storage must be provisioned before the job starts.
The proof must neither pull nor install anything and must not use `sudo` or
make any host mutation. A Docker shim, remote Podman service, rootful Podman,
fixture, mock, or fake executable is not a substitute for this prerequisite.

## Goal and Smallest Eventual Test/CI Scope

Once the prerequisite exists, add one deterministic runtime-backed M4 test and
one trusted required CI invocation for it—nothing in production code. The test
must run as the approved non-root user against the preloaded immutable digest
image with real local Podman, use `--pull=never` and `--userns=keep-id`, and
prove that the container's effective UID equals that non-root host user's
effective UID. It must fail if the host user is root, Podman is not operating
rootlessly, the image is unavailable locally at the approved digest, or the
observed container UID differs.

The trusted CI job must execute that test non-ignored on the approved runner,
record the immutable image digest and runner identity in its evidence, and be
required for the change that introduces the test. It must verify the existing
`/etc/subuid` and `/etc/subgid` setup without creating, editing, or otherwise
mutating it.

| File | Eventual scope |
|---|---|
| `crates/gobrowse-sandboxd/src/runtime.rs` | Add the single test-only real-local-Podman `keep-id` identity proof beside the existing runtime tests. |
| `.github/workflows/ci.yml` | Add only the trusted required non-ignored M4 job that runs that proof on the approved runner. |
| `PLAN.md` | Replace the completed M7 batch plan with this bounded M4 external-gate record. |
| `docs/implementation-progress.md` | Record the matching external block. |

No source, CI, or test change is authorized while the listed external
prerequisite is absent.

## Non-goals

- No Docker compatibility shim, remote daemon/service, rootful runtime, fake
  executable, fixture, mock, or simulated user namespace result.
- No image pull, runtime installation, privilege escalation, `sudo`,
  `/etc/subuid` or `/etc/subgid` edit, storage setup, host configuration, or
  other host mutation.
- No sandbox route enablement, deployment enablement, release claim,
  production behavior change, container-hardening claim, or expansion into
  network, escape, resource, or lifecycle coverage.
- No claim that existing fake or Docker-backed tests prove the rootless Podman
  `keep-id` user-namespace mapping.

## Stop Conditions and Evidence

Do not start the eventual test or CI work until every external prerequisite is
approved and present. Do not bypass a missing prerequisite with Docker,
rootful/remote Podman, a downloaded image, an install, `sudo`, a configuration
change, or a locally fabricated result.

When the approved runner exists, the only acceptance evidence is the focused
non-ignored real-local-rootless-Podman test and its trusted required CI job.
Until then, M4 remains `BLOCKED_EXTERNAL`, the sandbox remains release-gated
and off, and there is no M4 runtime-proof success claim.