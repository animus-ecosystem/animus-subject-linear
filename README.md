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

This backend advertises a single subject kind: **`issue`**. Animus addresses it via the kind-scoped routing contract — `issue/list`, `issue/get`, `issue/update`, `issue/create`. CLI calls use `--kind issue`:

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
| `subject/update`  | `issueUpdate` + `commentCreate`    | `patch.status`, `patch.assignee`, `patch.labels_add/remove`, `patch.custom` ride on `issueUpdate`. `patch.comment` posts a real Linear comment via `commentCreate` — it does **not** overwrite the issue body. See [Known limitations](#known-limitations) — `patch` carries no priority. |
| `subject/create`  | `issueCreate`                      | Since v0.1.8. Flat params `{title, body?, status?, priority?, labels?}` at the **top level** (not under `patch`). Requires `LINEAR_TEAM_ID`; optional `LINEAR_PROJECT_ID` files the issue into a project. `body`→`description`, `priority` bucket (`p0`–`p3`)→Linear int, `status`→discovered `stateId`. **Labels are accepted but dropped** (not yet applied on create). Registered for both `issue/create` and `subject/create`. |
| `subject/schema`  | static + runtime workflow states   | `kinds: ["issue"]`; `supports_create: true`; native states discovered lazily from the team's workflow.                                       |
| `health/check`    | `viewer { id name }`               | Returns `Unhealthy` (without hitting the network) when `LINEAR_API_TOKEN` is unset.                                                          |

## Known limitations

### Priority can be set on create but not changed on update

`issue/create` honors a `priority` bucket (`p0`–`p3`) and sets it on the new
Linear issue, but `subject/update` **cannot change the priority of an existing
issue** — a value passed as `animus subject update … --priority p1` never
reaches this plugin.

**Root cause (upstream, in animus-cli — not fixable in this plugin):** the
subject protocol's `SubjectPatch` type — the payload that `subject/update`
deserializes into — has **no `priority` field**. It models only `status`,
`assignee`, `labels_add`, `labels_remove`, `comment`, and `custom`
(`crates/animus-subject-protocol/src/lib.rs`). The CLI accepts `--priority` and
places it on the wire patch, but when the daemon deserializes that JSON into
`SubjectPatch`, the unmodeled `priority` key is silently dropped by serde. The
plugin's `update()` therefore never receives a priority, and `build_update_input`
has nothing to map.

**To address:** add a `priority` field to `SubjectPatch` upstream in
`animus-subject-protocol`. Once it exists, wire it through `build_update_input`
in `src/backend.rs` (reusing `priority_bucket_to_linear`, mirroring the create
path). No change here will help until the protocol carries the field.

### Linear's priority scale is reversed relative to Animus

Linear's `priority` integer runs **opposite** to Animus's `Subject.priority`
scale (Linear `1`=Urgent … `4`=Low; Animus `0`=none … `4`=critical), so
`linear_priority_to_animus` reverses it to keep "most urgent" aligned across
both (P0 = highest). This is gated behind the `ANIMUS_PRIORITY_REVERSE` const in
`src/backend.rs`; if Animus ever realigns its scale, flip that const to `false`
and update the pinned mapping tests.

### Labels are not applied on create

`issue/create` accepts a `labels` array but currently logs and drops it (label
name→id resolution is not yet implemented). `supports_create` is still advertised
as `true`. Labels can be added afterward via `subject/update` (`labels_add`).

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
- [x] `issue/create` / `subject/create` via `issueCreate` (since v0.1.8)
- [ ] Webhook support for real-time updates (`subject/watch`)
- [ ] Apply labels on create (name→id resolution) — see [Known limitations](#known-limitations)
- [ ] Honor priority on `subject/update` — **blocked upstream**: needs a `priority` field on `SubjectPatch` in `animus-subject-protocol` (see [Known limitations](#known-limitations))
- [ ] Release binaries (macOS aarch64/x86_64, Linux x86_64)

Follow the [Animus core repo](https://github.com/launchapp-dev/animus-cli) for protocol-level progress.

## License

MIT — see [LICENSE](LICENSE).
