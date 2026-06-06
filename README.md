# animus-subject-linear

A [Linear](https://linear.app) subject backend plugin for [Animus](https://github.com/launchapp-dev/animus-cli).

## What this is

Animus v0.4.0+ makes subjects (units of dispatchable work) pluggable. This repository ships `animus-subject-linear`, a standalone stdio plugin that exposes Linear issues as Animus subjects. Workflows dispatch agents over your Linear backlog without your team moving off Linear.

## Install

```bash
animus plugin install launchapp-dev/animus-subject-linear
export LINEAR_API_TOKEN=lin_api_…
export LINEAR_TEAM_ID=<your-team-uuid>   # required for status discovery + scoped queries
```

## Subject kind

This backend advertises a single subject kind: **`issue`**. Animus addresses it via the kind-scoped routing contract — `issue/list`, `issue/get`, `issue/update`. CLI calls use `--kind issue`:

```bash
animus subject list --kind issue --status ready
animus subject get  --kind issue --id linear:ENG-123
animus subject update --kind issue --id linear:ENG-123 --status in-progress \
    --comment "kicked off implementation"
```

Subject ids are namespaced as `linear:<identifier>` (e.g. `linear:ENG-123`) so the daemon can route writes back to this backend from the id prefix alone.

## Workflow YAML example

```yaml
# .animus/workflows/standard.yaml
subjects:
  linear-eng:
    plugin: animus-subject-linear
    config:
      api_token_env: LINEAR_API_TOKEN
      team: ENG
    status_map:
      ready:       ["Backlog", "Todo"]
      in_progress: ["In Progress", "In Review"]
      done:        ["Done", "Cancelled"]

workflows:
  - id: linear-impl
    subject_kind: issue
    phases: [...]
```

## Supported operations

| Method            | Backed by                          | Notes                                                                                                                                       |
|-------------------|------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| `subject/list`    | `issues(filter, first, after)`     | Pagination cursor returned in `next_cursor`.                                                                                                |
| `subject/get`     | `issue(id: $identifier)`           | `id` argument is the Linear identifier (e.g. `ENG-123`) or UUID.                                                                            |
| `subject/update`  | `issueUpdate` + `commentCreate`    | `patch.status`, `patch.assignee`, `patch.labels_add/remove`, `patch.custom` ride on `issueUpdate`. `patch.comment` posts a real Linear comment via `commentCreate` — it does **not** overwrite the issue body. |
| `subject/schema`  | static + runtime workflow states   | `kinds: ["issue"]`; native states discovered lazily from the team's workflow.                                                               |
| `health/check`    | `viewer { id name }`               | Returns `Unhealthy` (without hitting the network) when `LINEAR_API_TOKEN` is unset.                                                          |

The protocol does not yet expose a `subject/create` verb — see [`docs/architecture/subject-backend-plugins.md`](https://github.com/launchapp-dev/animus-cli/blob/main/docs/architecture/subject-backend-plugins.md) for the current contract. The schema's `supports_create` flag is reserved for the protocol expansion that adds it.

## Configuring status mapping

Linear lets every team customize the names of their workflow states (e.g.
`"Spec"`, `"Implementation"`, `"Code Review"`, `"Shipped"`), so a hardcoded
name map only works for teams using Linear's default template. Instead, the
plugin discovers the team's actual workflow at startup and auto-maps each
state to one of the Animus statuses (`Ready`, `InProgress`, `Blocked`,
`Done`, `Cancelled`).

### Auto-mapping (default)

On the first `list`/`get`/`update` call the plugin queries Linear's
`team.states.nodes { id name type position }` and uses the **`type`**
field to map every state. The `type` is fixed by Linear regardless of
what the team renames the state to:

| Linear `WorkflowState.type`                | Animus `SubjectStatus` |
|--------------------------------------------|------------------------|
| `triage`, `backlog`, `unstarted`           | `Ready`                |
| `started`                                  | `InProgress`           |
| `completed`                                | `Done`                 |
| `cancelled`                                | `Cancelled`            |

Unknown future types default to `Ready` so a Linear-side addition won't
freeze your dispatch loop.

### Overrides via `LINEAR_STATUS_MAP`

If your team uses semantics that don't match the type-based mapping
(for example, you want `"Code Review"` to count as `Done` rather than
`InProgress`), set the `LINEAR_STATUS_MAP` env var to a JSON object
keyed by Linear state **name** (case-sensitive):

```bash
export LINEAR_STATUS_MAP='{
  "Spec":         "Ready",
  "Implementation": "InProgress",
  "Code Review":  "InProgress",
  "Shipped":      "Done"
}'
```

Values must be one of `Ready`, `InProgress`, `Blocked`, `Done`,
`Cancelled` (PascalCase or kebab-case both accepted). Unknown values
are silently skipped — the rest of the map still applies. A malformed
JSON blob falls back to the type-based auto-map and logs a warning.

### Ambiguity resolution on the write path

If multiple Linear states map to the same animus status (e.g. both
`"Spec"` and `"Backlog"` map to `Ready`), `update()` picks the one with
the **lowest `position`** — Linear's default "first" state for that
category. This keeps writes deterministic without forcing you to
disambiguate in `LINEAR_STATUS_MAP`.

### Why `stateId` and not `stateName`?

The Linear API takes a `stateId` (UUID) on `issueUpdate(input: { stateId })`.
Sending a name string is tolerated but not the documented shape and breaks
if two teams have a state with the same name. This plugin always sends
the UUID it discovered for the team in question.

## Design

The subject backend plugin protocol is defined in the Animus core repo:

- **Protocol design:** [`docs/architecture/subject-backend-plugins.md`](https://github.com/launchapp-dev/animus-cli/blob/main/docs/architecture/subject-backend-plugins.md)
- **Naming contract:** [`docs/architecture/naming-contract.md`](https://github.com/launchapp-dev/animus-cli/blob/main/docs/architecture/naming-contract.md)
- **Repository name:** `animus-subject-linear`
- **Crate name (published to crates.io):** `animus-subject-linear`
- **Binary name:** `animus-subject-linear`

Per the v0.4.0 naming convention: repo, crate, and binary all share the same `animus-{kind}-{name}` name. There is no longer an `ao-` prefix anywhere.

## Roadmap

- [x] `SubjectBackend` trait implementation against Linear's GraphQL API
- [x] Status mapping (auto-discovered from team workflow; `LINEAR_STATUS_MAP` overrides)
- [x] Authentication via `LINEAR_API_TOKEN` env var
- [x] Pagination
- [x] `patch.comment` posts a Linear comment via `commentCreate` (since v0.1.5; earlier versions incorrectly overwrote `description`)
- [ ] Webhook support for real-time updates (`subject/watch`)
- [ ] `subject/create` (waiting on protocol expansion; tracked in [animus-cli](https://github.com/launchapp-dev/animus-cli))
- [ ] Release binaries (macOS aarch64/x86_64, Linux x86_64)

Follow the [Animus core repo](https://github.com/launchapp-dev/animus-cli) for protocol-level progress.

## License

MIT — see [LICENSE](LICENSE).
