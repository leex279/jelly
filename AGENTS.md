# jelly: agent notes

Read `README.md` for what jelly is. Vocabulary: **jelly** (one live app: view, state,
verbs), **jelly page** (the internal jelly, id `surface`, that lays out the others),
**attention** (asks and looks for the person), **verb**, **reshape**,
**event**. Do not call a jelly an app, widget, canvas or artifact.

## Invariants

- Agents show, people react. An agent lays out options with tradeoffs, previews and
  a recommendation; the person picks, says why, or comments. Never hand the person
  an empty input field.

- State changes only through verbs. Do not add an API that writes state directly;
  undo, events and attribution all depend on it.
- Verbs run one at a time under the store lock in `server.rs`.
- Reshape and create refuse verbs that do not load (`verbs::check`).
- An event's `state` field goes to live listeners only, never into `events.jsonl`.
- Browsers get events over one WebSocket per page (`/api/ws`). Do not give views
  `EventSource`s: each one holds an HTTP connection, and a page with a few jellies
  hits the browser's six-per-host limit and stalls every request.
- Comments (`comments`) and attention items live beside state, not in it. Undo does not touch them.
- A jelly's id is `project:name`; the project is the part before the colon. Short
  names resolve in the CLI (current project) and the runtime (the view's project).
- Assets live in `~/.jelly/assets/<id>/`, outside state and undo. On Windows the folder
  is `project+name`, because `:` is not allowed in a file name (`disk::name`). Views are
  served at `/j/<id>/` so `assets/<file>` resolves; keep that trailing slash.
- Never put network access in verbs. Live data is pushed in with `jelly call`.

## Check

```bash
cargo test
cargo clippy
```

For a manual run, use a throwaway home and port so your real jellies are untouched:
`jelly serve --port 7879 --home /tmp/jelly-test` (PowerShell:
`jelly serve --port 7879 --home $env:TEMP\jelly-test`), then pass `--url http://127.0.0.1:7879`.
