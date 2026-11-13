# jelly

Allow your agent to show you what it means instead of telling you: a prototype you
can click, a choice with the options side by side, a plan, a dashboard, a 3D
render, a question you answer in one tap. You use it, comment on any part of it,
and your agent hears everything you did. jelly runs on your machine and works with
any agent that has a shell.

## For agents

jelly gives you a place to put jellies in front of the person you work for: small
live pages you write in minutes that they use, answer and shape with you. Reach for
it whenever a picture, a prototype, a comparison or a choice would land better than
prose, and whenever you need them to choose.

- **Show anything.** A jelly is a view (any HTML), state (JSON) and verbs (the JS
  functions that change state). Prototypes, three.js, charts, forms, animations.
  No component kit, no ceiling.
- **Hear their answer.** Every click is a verb call with an author. `jelly ask` and
  `jelly look` put a question on the exact element it is about. Comments attach
  to elements. You get all of it from `jelly inbox` (or a hook at the start of your
  turn), or block on `jelly wait`.
- **Get choices made.** `jelly ask --spec` lays out options with live previews,
  gains, costs and your recommendation. They pick, add a note, or add their own question.
- **See what you made.** `jelly see NAME` prints the page as text, with everything
  waiting on them and what they did lately. `--png` when layout matters.
- **Keep working on it together.** Reshape while it runs and state stays. Write
  state yourself with `jelly set`. Undo the last change. Notes for the next agent. One page per project. Share as one file.

```bash
jelly ls                                         # list jellies; starts the daemon on first use
jelly new plan --view view.html --verbs verbs.js --about "One line: why this exists"
jelly focus plan                                 # put it in front of them
jelly ask plan "Which layout?" --option dense="Dense" --option roomy="Roomy" \
  --recommend roomy --why "Easier to scan on a phone" --at "#grid"
jelly inbox                                      # what they did since you last looked
jelly see plan                                   # what is on screen, as text
```

When piping into `head` or `grep -q`, stdout BrokenPipe ends the command quietly.
Finite commands finish their changes before printing. `events`/`watch` and `dev`
stop when an output write detects closure; while idle, they keep waiting. `wait`
keeps its event selection and timeout behavior, detecting closure only when it
prints a qualifying event. `serve` continues serving when stdout closes.

The full guide is [`skill/SKILL.md`](skill/SKILL.md). Install it as a skill so every
session knows it: `ln -s "$PWD/skill" ~/.claude/skills/jelly` (PowerShell:
`New-Item -ItemType Junction -Path "$HOME\.claude\skills\jelly" -Target "$PWD\skill"`).

## For the person: install

```bash
cargo install --path .
jelly ls               # starts on first use: http://127.0.0.1:7878
```

The first client command starts a detached daemon if needed, waits for it, and
prints one startup line on stderr. Data lives in `$JELLY_HOME` or `~/.jelly`, just
as with `jelly serve`; on Windows `~/.jelly` is `%USERPROFILE%\.jelly`. No login
service or extra install step is needed.

`jelly serve` still runs in the foreground for manual use and startup diagnostics.
Unreadable or malformed persisted jellies prevent startup rather than serving a
partial store. Run `jelly serve` to see the affected file and error.
For an isolated run: `jelly serve --port 7879 --home /tmp/jelly-test` (PowerShell:
`--home $env:TEMP\jelly-test`), then use
`jelly --url http://127.0.0.1:7879 ls`. On-demand startup applies only to the default
local endpoint, including `http://localhost:7878` and equivalent URL spellings;
other `--url` / `JELLY_URL` targets must already be running.

When upgrading, existing homes keep their stored jelly page, including any
customizations. To install the bundled dock improvements and responsive stream and
full-view headers, run this from the updated checkout against your running daemon:

```bash
jelly reshape surface --view assets/surface.html
```

This replaces the jelly page's view and keeps its state. If you customized that
view, incorporate the improvements from `assets/surface.html` into your version
and reshape with that file instead. There is no automatic upgrade or compatibility
layer for stored jelly pages.

To let agents hear you without asking, add `jelly inbox --hook` as a Claude Code
`UserPromptSubmit` hook. It starts the default daemon silently, suppresses startup
errors, and prints nothing when there is nothing new. Outside a project it still
attempts startup, then exits successfully without inbox output. Once startup
succeeds, a daemon that does not answer the inbox request within a second, reader
identification, decoding and daemon read or progress-commit failures appear in
hook output as `jelly inbox failed: ...`. Failed reads do not
advance progress. Claude's
`CLAUDE_CODE_SESSION_ID` environment variable keeps one inbox reader across
commands and resumed conversations. JSON `session_id` identifies hook invocations
only. To share that progress with ordinary commands, supply the same conversation
ID through `inbox --session-id` or `JELLY_SESSION_ID` (or use the same explicit
`--as` / `JELLY_AS` name for both).

Inbox identity uses explicit `--as` / `JELLY_AS` first, then
`inbox --session-id` / `JELLY_SESSION_ID`, then `CLAUDE_CODE_SESSION_ID`, then
`session_id` from hook stdin. Other agents should export a stable, distinct
conversation token as `JELLY_SESSION_ID`. Without one, the invoking parent
process's PID and start time identify the reader for that process's lifetime;
a new parent starts a new reader. Shared names or conversation tokens intentionally
share progress. Event attribution still defaults to `agent`.

Progress is stored in the daemon's home (`inbox-readers.json`), per reader and
project, and survives daemon restarts. A new reader sees the last hour; later
reads use event positions. Upgrading leaves old client `inbox/` timestamps unused,
so each new reader can replay the recent hour. Use a matching CLI and daemon.
The non-consuming POST `/api/inbox` accepts `{ "project": "...", "reader": ... }`
with reader forms `{ "kind": "named", "name": "..." }`,
`{ "kind": "conversation", "id": "..." }`, or
`{ "kind": "parent", "pid": 123, "start_time": 456 }` (Unix seconds).
Omitting `project` or using an empty string reads all projects. The response is
`{ "events": [...], "through": 123 }`; events include `text`. The first read
registers a fixed one-hour starting window, so uncommitted events remain available
on retry even after that hour passes. After decoding and successfully writing and
flushing output, the CLI sends POST `/api/inbox/commit` with the same project and
reader plus the returned `through` position. Commits only move that reader's
progress forward and leave events newer than `through` unread. Lost or undecodable
read responses do not consume events. Ambiguous output, failed acknowledgements
or client crashes may replay events. GET `/api/inbox?project=...&since=...` remains
a non-consuming timestamp query returning an array.

The daemon refuses startup if an existing event log cannot be read, contains
malformed events or is nonempty and missing its final newline. This prevents
reusing positions already passed by inbox readers or corrupting the final record
on the next append.

`~/.jelly/events.jsonl` is trimmed once it passes 8 MiB, to its newest 4 MiB plus up
to 1 MiB of your older events, so the inbox and `jelly see` still have them. A trim
keeps every event's position, and inbox reads go back only as far as the reader's
position. `jelly see` uses the last change and last eight person events per jelly,
kept in memory during startup validation and updated on append and trim; it does
not scan the log on each request. Initial one-hour inbox windows and timestamp
queries use an in-memory timestamp index to read only matching event byte ranges,
then return them in sequence order, even when the wall clock moves backwards.

Upgrading changes how a jelly's file stores undo: it now keeps each change's
difference instead of a full copy of view, verbs and state. The new daemon reads
old files and keeps their undo, and rewrites each file in the new form at that
jelly's next change. Converted history is exempt from the new byte budget until
new history displaces it, within the same 100-step limit. An older binary cannot
read a rewritten file: it skips that jelly with a `missing field` error. To go
back to an older binary, keep a copy of `~/.jelly/jellies/` from before the upgrade.

`jelly pull` gets asset names and bytes from the selected daemon, regardless of
its home or the client's home. GET `/api/jellies/{name}/assets` returns a sorted
JSON array of filenames, `[]` for an existing jelly without assets, or a JSON 404
error for an unknown jelly. Directory enumeration errors return a JSON 500 error.
Bytes come from `/j/{name}/assets/{file}`. Pull reports
success only after every listed file is written; inventory or download failures
return an error and may leave a partial destination folder.

## How it works

A local daemon holds every jelly and serves them on 127.0.0.1 only, without
authentication: anything that can reach the port can run verbs. Verbs run one at a
time in QuickJS with a 1 second, 64 MB limit and no network, which keeps every
change attributable and undoable. Live data is pushed in from outside with
`jelly call`. Contributors: read [`AGENTS.md`](AGENTS.md).

Browser snapshots belong to the current served view. The daemon supplies ownership
evidence in view boot data and the bundled runtime echoes it when posting a
snapshot. Missing evidence is rejected; stale evidence returns 409 and the runtime
reloads. After upgrading the daemon, reload older open pages and update tokenless
snapshot callers before they can contribute snapshots again. This is an internal
daemon/runtime protocol, not authentication.
