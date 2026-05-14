# animus-subject-linear

A [Linear](https://linear.app) subject backend plugin for [Animus](https://github.com/launchapp-dev/animus-cli).

> **Status:** Under construction — landing in Animus v0.4.0.

## What this is

Animus v0.4.0 makes subjects (units of dispatchable work) pluggable. This repository will ship `ao-subject-linear`, a standalone stdio plugin that exposes Linear issues as Animus subjects. Workflows dispatch agents over your Linear backlog without your team moving off Linear.

Once published, you'll be able to:

```yaml
# .ao/workflows/standard.yaml
subjects:
  linear-eng:
    plugin: ao-subject-linear
    config:
      api_token_env: LINEAR_API_TOKEN
      team: ENG
    status_map:
      ready:       ["Backlog", "Todo"]
      in_progress: ["In Progress", "In Review"]
      done:        ["Done", "Cancelled"]

workflows:
  - id: linear-impl
    subject_type: linear-eng
    phases: [...]
```

## Design

The subject backend plugin protocol is defined in the Animus core repo:

- **Protocol design:** [`docs/architecture/subject-backend-plugins.md`](https://github.com/launchapp-dev/animus-cli/blob/main/docs/architecture/subject-backend-plugins.md)
- **Naming contract:** [`docs/architecture/naming-contract.md`](https://github.com/launchapp-dev/animus-cli/blob/main/docs/architecture/naming-contract.md)
- **Repository name:** `animus-subject-linear`
- **Crate name (published to crates.io):** `animus-subject-linear`
- **Binary name:** `animus-subject-linear`

Per the v0.4.0 naming convention: repo, crate, and binary all share the same `animus-{kind}-{name}` name. There is no longer an `ao-` prefix anywhere.

## Roadmap

- [ ] `SubjectBackend` trait implementation against Linear's GraphQL API
- [ ] Status mapping configurable via workflow YAML
- [ ] Authentication via `LINEAR_API_TOKEN` env var
- [ ] Pagination
- [ ] Webhook support for real-time updates (`subject/watch`)
- [ ] Contract test against `ao-subject-mock`
- [ ] Release binaries (macOS aarch64/x86_64, Linux x86_64)

Follow the [Animus core repo](https://github.com/launchapp-dev/animus-cli) for v0.4.0 progress.

## License

MIT — see [LICENSE](LICENSE).
