---
name: jelly
description: Show things to the person instead of writing walls of text. Use jelly whenever a picture, a prototype, a live dashboard, a plan, a diff, a comparison or a choice between options would explain better than prose, and whenever you need the person to choose. Also use it to read what the person did or said in a jelly, to change one while it runs, or when they say "show me" or "jelly".
---

# jelly

jelly is how you show the person things. A jelly is a small live page you write in
minutes: a clickable prototype, an animated explainer, a 3D scene, a dashboard, a
plan, options side by side. The person sees it on the jelly page
(http://127.0.0.1:7878), uses it, answers your questions and comments on any part
of it. You hear all of it.

**Show, then let them react.** Never hand the person an empty question. Do the
thinking: lay out the options, what each gains and costs, what each looks like,
and which you recommend. Their job is to react, not to write your spec.

Use jelly directly: the first client command starts the default daemon in the
background and waits for it, printing one startup line on stderr. `jelly ls`
lists this project's jellies (`--all` for every project).

Startup uses the same home as `jelly serve`: `$JELLY_HOME` or `~/.jelly`, at
http://127.0.0.1:7878 (also localhost:7878 and equivalent spellings). Other
`--url` / `JELLY_URL` targets must already be running.
If startup fails, run `jelly serve` in the foreground to see the cause.
`jelly inbox --hook` starts silently, suppresses startup errors, and prints only
new inbox items. Outside a project it attempts startup and exits successfully
without inbox output.

## Make a jelly

Write three files in a scratch folder, never in the person's repo:

- `view.html`: any page. `window.jelly` exists before your scripts run.
- `verbs.js`: `const verbs = { name(state, args, { by, at_ms }) { … } }`. Verbs are
  the only way state changes; a button and you call the same verb. Change `state`
  in place or return a new one; throw to refuse. `const verbs = {};` for a page
  that only shows.
- `state.json`: the starting state.

```bash
jelly new plan --view view.html --verbs verbs.js --state state.json --about "One line: why this exists"
jelly new plan --from ./plan         # the same from a folder: a recipe, or what `jelly pull` wrote
jelly reshape plan --view v2.html    # new view or verbs, same state; open pages reload
jelly call plan add '{"title":"…"}'  # run a verb yourself; '@args.json' or - for big args
jelly set plan --at /title '"Better title"'   # write state yourself, no verb needed
jelly set plan --at /draft --rm      # remove a key; `jelly set plan @state.json` replaces it all
jelly undo plan                      # undo the last change to view, verbs or state
jelly pin plan                       # keep it; unpinned jellies archive after a quiet week
jelly rm plan
```

In the view:

```js
jelly.onState((state, event) => render(state)); // now, and after every change by anyone
jelly.call("add", { title });                   // runs a verb; rejects with its error
jelly.read("other"); jelly.watch("other", fn);  // another jelly's state
jelly.focus("other");                           // open another jelly
jelly.asset("shot.png");                        // URL of a file added with jelly asset
jelly.itemsAt("#thumbs")[0]?.answer?.option;    // what they picked in a question about #thumbs
const stop = jelly.onAttend(item => {             // when the dock opens this question
  if (item.anchor?.selector === "#details-title") {
    document.querySelector("#details").hidden = false;
  }
});
// stop() unsubscribes; registration does not call the handler.
```

Keep data in state, not in page variables or localStorage, so you and the person
see the same thing. Give elements stable `id`s: questions and comments point at them.

### Place the Comment button

A standalone jelly (`/j/<id>/`) has a Comment button 10px from the bottom-right
corner. Reserve a clear spot for it when your view has fixed or sticky controls.
Set `--jelly-comment-inset` on `:root` or `body`; the button inherits it. The value
uses CSS `inset` order: top, right, bottom, left. Use all four values, with `auto`
for the opposite edges, so the button keeps its natural size.

```css
/* Above a bottom control bar. */
:root { --jelly-comment-inset: auto 10px 78px auto; }

/* Top right. */
:root { --jelly-comment-inset: 10px 10px auto auto; }

/* A bottom bar whose height the view already measures. */
:root { --jelly-comment-inset: auto 10px calc(var(--foot, 68px) + 10px) auto; }

/* Move it only while a mobile tracing panel is open. */
@media (max-width: 900px) {
  body.tracing { --jelly-comment-inset: 10px 10px auto auto; }
}
```

Media queries, class changes and updated CSS variables move the button immediately.
Placement follows normal CSS rules; the runtime does not detect obstacles. Check
that Comment and your controls remain visible and clickable at supported sizes
and states, including the longer label while commenting.

On the jelly page, the header supplies Comment and embedded jellies omit their
local button. A view with its own comment control can still omit the standalone
button with `<meta name="jelly-comment" content="off">`; Option-click remains
available. That metadata does not set placement.

## Recipes

Optional starting points for jellies that are easy to get wrong the first time.
A recipe is a folder next to this file with `view.html`, `verbs.js`, an example
`state.json` and a `RECIPE.md`. Read its `RECIPE.md` only when you use it, copy
the folder to a scratch place, write your own state, and change the view and
verbs as the job needs: `jelly new review --from /tmp/review`.

- `recipes/review`: you changed code and the person must judge it. A map of what
  they asked for wired to the changes, before and after per change, approve or
  push back, then go.
- `recipes/plan`: work with more than three steps. A live track of steps the
  person can reorder, veto or put a gate on; you start, show progress and finish.
- `recipes/triage`: ten to a hundred items the person must agree on. You sort
  them into lanes; they correct the ones you were unsure of.
- `recipes/map`: how a system fits together. Layers, nodes and wires, traces that
  play step by step, and the nodes a change will touch.
- `recipes/scope`: a tradeoff between many parts. Items in or out, a few levers,
  and a date and capacity bar that move as the person plays.
- `recipes/timeline`: working out why something happened. Evidence on a time
  axis, competing explanations, and the person says which they believe.
- `recipes/monitor`: something that runs for minutes or hours. Live charts fed
  by `jelly call`, and pause, stop and limits the person sets and your loop obeys.

## Show it

```bash
jelly working plan "rebuilding the header"   # the person sees it and waits
jelly done plan
jelly see plan                 # check your work: the page as text, plus what waits on the person
jelly see plan --width 390     # at phone width; add --png when layout matters
jelly see plan --full          # screenshot the whole page, not just the first screen
jelly focus plan               # open it full size on their screen
```

Always look before you show: run `jelly see`, and fix what is off before you focus
the jelly or ask about it. `see` lists the errors your page threw (`error` lines)
and questions whose `--at` has invalid CSS selector syntax or matches nothing (`!`
lines): fix them. `jelly ask` and
`jelly look` say in their reply where the card landed: below the element, at the
top because the selector is invalid, nothing matched or the element is in a fixed
or sticky part, or below
an element that is not on screen right now. If adjacent placement is unavailable,
the card is shown at the top and the reply says so after the browser measures it.
If row insertion is unsafe, the reply and `see` name the actual fallback destination.
Without a measurement for that question, the reply says it was not checked. Pages
that render their own inline attention retain invalid, missing, floating and hidden anchor
diagnostics; the reply marks placement as not checked because the view places its
own attention, as does `see` when reporting selector warnings. Clear your own test clicks with `jelly set` before the person uses the jelly. Add `assets/poster.png`, a frame that tells the story on
its own, and the jelly page uses it as the thumbnail.

## Ask the person: attention

Attention is everything you need from the person. Each question shows as a card
in the page itself, below the element `--at` points to (or at the top), and
in their attention dock on the jelly page. They answer in place; an answered card
shrinks to one line they can change. In a grid or wrapping flex group, the card
spans the group's width after the item's row, before later rows. List items and
table rows get a full-width question row too. An anchor inside an SVG puts the
card below the whole drawing. Layouts that cannot take a question row keep their
layout; the reply names the group the card follows, or says it went to the top.
If its anchor or adjacent card is hidden or clipped, the same card appears at the top with a line naming the part it is about.
It returns beside the anchor when that part appears, keeping the draft and focus.

`jelly.onAttend(fn)` notifies the view synchronously with the attention item before
placement and focus, so it can reveal the part identified by `item.anchor.selector`.
It returns an unsubscribe function. The runtime shows a fallback immediately;
asynchronous reveals never delay it, and later DOM changes relocate the card.
A presentation-only reveal can change the DOM as above; persistent selection
changes still go through a verb with `jelly.call`.

One question on an existing jelly, about one part of it:

```bash
jelly ask plan "Which hero direction?" --option bold="Bold gradient" \
  --option quiet="Quiet white|Swaps to a plain hero" --recommend quiet \
  --why "Calmer, and the button stands out more" --at "#hero" --wait
# blocks; prints: a1 answered: quiet · "keep the button blue"
jelly ask plan "Anything missing before I ship?"     # answered in words
jelly look plan "Check the footer links" --at "#footer"   # they answer "seen" or with a word
```

`--wait` is the recommended way to ask one question: it blocks until the person answers
(or the ask is withdrawn) and prints only that, with `--json` for the whole item.
`--timeout SECONDS` gives up with exit code 2. Without `--wait`, the answer arrives in
`jelly inbox` or `jelly wait`. To work between asking and waiting, keep the ask's token:

```bash
jelly ask plan "Anything missing before I ship?"
# If that ask printed "since seq:42", copy its token into wait:
jelly wait plan --since seq:42 --timeout 60 --json
# Inspect kind and args.id; if this is not the answer, continue from its seq.
```

Several related questions, with options laid out side by side, make an ask page:

```bash
jelly ask sharing --spec spec.json --json   # asking again starts a new round; open questions are withdrawn
```

Its JSON reply includes `jelly`, `round`, `since` (for example, `"seq:57"`) and
`next`, with a concrete `jelly wait sharing --since seq:57 --json` command. Use the
actual jelly name and token from that reply.

```json
{
  "title": "Sharing a jelly with a colleague",
  "context": "Why these matter, in a short paragraph.",
  "questions": [
    { "id": "host", "question": "Where should shared jellies live?", "context": "…",
      "recommend": "relay", "why": "One sentence on why",
      "options": [
        { "id": "relay", "label": "A small hosted relay", "does": "One or two sentences.",
          "gains": ["…"], "costs": ["…"], "effort": "M · two days",
          "preview": "<!doctype html>… any HTML: a mockup, an animation, a chart, a 3D scene …" }
      ] }
  ]
}
```

- Every question is an attention item, answered with an option, words, or both.
  "None of these" is an answer in words: read it, and ask a better round.
- Make every preview real and different. It runs live in a sandbox: scripts, CSS
  animation, canvas, WebGL and CDN imports all work. Two to four options.
- Recommend one, and say why. If you could have decided yourself, do not ask.
- `jelly withdraw plan a2` when a question no longer applies.
- `jelly call sharing applied '{"note":"Built in 3f2a1c"}'` when you acted on the answers.

## Hear the person

If the person has the inbox hook, what they did since your last turn arrives at
the start of your turn ("The person acted on your jellies…"). Act on it. Otherwise:

```bash
jelly inbox                  # what they did in this project since you last asked
jelly attention              # what still waits on them
jelly see plan               # includes their answers and recent actions
jelly wait plan --since seq:42 --timeout 60 --json  # use the token from your ask
jelly wait [plan]            # future events only; --json includes live state
jelly events [plan]          # stream every event as JSON lines
```

`attention`, `comments` and `wait` print one line per item; add `--json` for the full data.

`ask`, `look`, `call` and `reshape` print `since seq:N` from their own change.
Copy that token when you wait. `--since` returns the earliest recorded event
strictly after N, by someone other than your `--as` / `JELLY_AS` identity. Leave NAME out to read across
jellies, excluding the jelly page's own changes. Explicitly naming `surface`
includes those changes.

With `--json`, this mode prints one recorded event, without `state`. Other changes can arrive
before the answer. Check for `kind: "answer"` and the attention item's `args.id`;
read its `args.option` and `args.text`. For a spec round, also match `args.group`
to `round-N` using the `round` returned by `ask --spec`.

The spec question ID is an anchor, not the answer ID: `host` becomes `#q-host`.
Run `jelly show sharing` and find the entry in `attention` whose
`anchor.selector` is `#q-host` and whose `group` is the returned round, such as
`round-1`. Copy that entry's `id`, such as `a1`, to match the answer's `args.id`.
`jelly see sharing` also shows IDs and anchors for questions still waiting;
`show` includes already answered items too. IDs advance across rounds, so obtain
this mapping for each new round.

An empty look answer means "seen". If an event is not the answer you need, handle it and
wait again using its `seq`: after `"seq": 58`, use `--since seq:58 --json`. Continue
until the relevant answer arrives (all required answers for a spec round) or your
time budget expires. Each invocation's `--timeout` starts a new timeout, so use
the remaining budget when continuing. An answer completed before connection is
retained and reached this way even when other events precede it.

For current state, run `jelly state NAME` separately. Tokens use this daemon's
retained history, survive restarts and jelly replacement, and do not belong to another home.
`seq:0` reads from the beginning; a token beyond the current sequence waits for
a later event. Without `--since`, wait only hears future live events. A timeout
exits 2 with no event on stdout; no timeout means wait indefinitely.

Comments: the person can Option-click any element of any jelly and say what should
change. Answer on the same thread, and change the jelly if they asked for it:

```bash
jelly comments
jelly reply plan c3 "Split it into two steps." --resolve   # done: closes the thread
jelly reply plan c3 "Which date, the 12th or the 19th?"    # asking back: stays open
```

Set `JELLY_AS=<your name>` when several agents share one jelly page. Each distinct
name reads independently; reusing a name shares progress. Without an explicit
`--as` / `JELLY_AS`, inbox uses `--session-id` / `JELLY_SESSION_ID`, then
`CLAUDE_CODE_SESSION_ID`, then JSON `session_id` from `--hook` stdin. Use a stable,
distinct `JELLY_SESSION_ID` for other agents' conversations and reuse it on resume.
Without a supplied session ID, the invoking parent's PID and start time identify
the reader until that process exits; a new parent starts a new reader.

Cursors live in the daemon's home, per reader and project, and survive restarts.
New readers see the last hour, then receive events since their own last read by
event position. Old client timestamp cursors are unused after upgrading, so the
recent hour may replay once for each new reader. Event attribution still defaults
to `agent`. Use matching CLI and daemon versions. Inbox progress advances only
after decoded events are successfully written and flushed to output. Lost or
undecodable read responses leave events available for retry; ambiguous output,
failed progress commits or client crashes may replay events. Hook diagnostics
report a daemon that does not answer, reader identification, decoding and daemon
read or commit failures; an empty inbox or no running daemon stays quiet.

## Projects and names

A jelly is named `project:name`. The project is the git repo you run `jelly` in
(or `--project`, `JELLY_PROJECT`). Use short names: `jelly new plan` in the archon
repo makes `archon:plan`, and `jelly call plan …` there finds it. Elsewhere use the
full name. Inside a view, `jelly.read("plan")` means this project's plan. The
person sees each project on its own page, `/p/<project>`. `jelly move NAME OTHER`
moves a jelly; `jelly restore NAME` brings back an archived one.

## Files, assets, design, sharing

```bash
jelly note plan "Cards move next→doing→done; the person decides what moves to doing."  # for the next agent
jelly pull plan ./plan           # view.html, verbs.js, state.json, assets/
jelly dev plan ./plan            # changes the jelly on every save
jelly asset plan shot.png demo.mp4 model.glb   # beside the jelly; use as assets/shot.png
jelly design                     # the project's DESIGN.md, if the repo has one
jelly share plan                 # one read-only HTML file, state and assets baked in
jelly push plan --to URL         # a live copy on a hosted instance that follows this jelly (key: `jelly remote add URL KEY`)
jelly unshare plan               # stop syncing it; --rm also removes the copy there
```

Shared files include every uploaded asset, including names read from state or
resolved with `jelly.asset(file)`. They open offline from `file://` and keep state
values unchanged. `items`, `attention`, `itemsAt(selector)` and `onAttention(fn)`
read the attention captured when shared; `onAttention` runs once immediately.
Answers and other mutations refuse changes and show the read-only note.
For dynamic `<object>` resources, assign `object.data = jelly.asset(file)` (also
when the filename comes from state). Shared files do not automatically rewrite
object data assigned by scripts or inserted markup. Static `<object data="assets/file">`
tags in the view HTML, including declarative shadow roots, are embedded during
export when they resolve to the uploaded asset independently of the output file's
location. External or location-dependent object URLs stay unchanged. Inert template
content stays unchanged. Images named in state continue to resolve automatically.
External dependencies and other jellies are not bundled.

A repo's DESIGN.md (github.com/google-labs-code/design.md) becomes CSS variables at
`/p/<project>/design.css` (`--color-primary`, `--rounded-md`, …). Use it when it
helps the jelly feel like the product. Do not inline big files as base64; add them
as assets.

## Live data

Verbs have no network. To keep a dashboard live, push data in from outside, from
your own turn, a script or a cron job: `jelly call dash add '{"t":…,"v":…}'`. The
view can also fetch public APIs itself in the browser.

## Limits

A verb runs at most 1 second and has no file, network or process access. The view
has the full browser. By default the daemon listens on 127.0.0.1 only and has no
login; a hosted instance (`jelly serve --hosted`) needs the owner's key for every
change and none for reading.

If deleting a saved jelly file fails during `jelly rm` or `jelly move`, the old
jelly can reappear after a daemon restart. Saved files and recorded history
can then disagree about which jelly identities exist.
