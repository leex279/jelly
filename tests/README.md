# Attention visibility browser check

Requires Cargo, Chrome and `playwright-cli`. Use a throwaway daemon home. From the
checkout, build and install the fixture (substitute your own scratch path/port):

```sh
cargo build --locked --target-dir /tmp/jelly-visibility-target
printf 'const verbs = {};\n' > /tmp/jelly-visibility-verbs.js
/tmp/jelly-visibility-target/debug/jelly serve --port 17881 --home /tmp/jelly-visibility-home
# In another terminal, from the checkout:
/tmp/jelly-visibility-target/debug/jelly --url http://127.0.0.1:17881 new visibility:fixture \
  --view tests/attention-visibility.html --verbs /tmp/jelly-visibility-verbs.js
```

Run the browser from a scratch directory so generated browser files stay outside
the checkout. A home created by an earlier build retains its stored jelly page;
use a fresh home, or update that test page with
`/tmp/jelly-visibility-target/debug/jelly --url http://127.0.0.1:17881 reshape surface --view assets/surface.html`
from the checkout before running. The program takes the daemon URL from the open page and creates
fresh jellies through the API. Use an absolute path to the test:

```sh
playwright-cli -s=visibility open http://127.0.0.1:17881/p/visibility
playwright-cli -s=visibility resize 1280 900
playwright-cli -s=visibility run-code --filename=/path/to/jelly/tests/attention-visibility.js
playwright-cli -s=visibility resize 390 844
playwright-cli -s=visibility run-code --filename=/path/to/jelly/tests/attention-visibility.js
playwright-cli -s=visibility close
```

The check clicks real dock rows and asserts visibility, shadow-root focus,
context, measured diagnostics, host/draft/selection preservation, synchronous and
delayed reveals, subscription behavior, stable shared destinations, persisted
answers, grouped inline questions with empty and punctuation IDs, readable grouped
question headings alongside visible focus below the sticky bar, tall preview and
long-context openings of later grouped questions with focus inside the requested
question and without unseen Enter submissions, hash/query/path
history navigation, attribute-driven opacity, visible refresh and focus-write
failures with retry, delayed focus writes and agent
focus during local open/close writes, and snapshot registration.
Bounded-section reveals check that fallback context does not prevent the normal
card from returning to its anchor. Overlapping opening and comment refreshes
check that an opening waits for committed attention, including HTTP and network
failures of the newest request.
Hidden-section visibility fails on the pre-fix runtime. Stop the throwaway daemon
after testing; no real jelly home is used.

# Full-view header browser check

Requires Cargo, Chrome and `playwright-cli`. Build from the checkout and start a
fresh throwaway daemon (substitute your scratch paths and port):

```sh
cargo build --locked --target-dir /tmp/jelly-header-target
/tmp/jelly-header-target/debug/jelly serve --port 17849 --home /tmp/jelly-header-home
```

From a scratch directory, run the check with an absolute path to the script:

```sh
playwright-cli -s=header open http://127.0.0.1:17849/
playwright-cli -s=header run-code --filename=/path/to/jelly/tests/full-view-header.js
playwright-cli -s=header close
```

The script creates and cleans up its own fixtures. It uses a mobile browser context
at 390px and checks that the layout viewport matches the device width at full scale.
It checks title and metadata geometry, action reachability, wrapping and truncation
at 320, 390, 900, 901 and 1280px. At phone and desktop widths it also checks
Comment, iframe Escape, the parent c shortcut, Undo, tab order, Pin/Unpin, Back,
Enter, Escape and Remove.
It returns measurements and saves `header-390.png` and `header-1280.png` in the
scratch directory. Stop the throwaway daemon afterward. Existing homes retain
their stored surface; use a fresh home for the newly built bundled view.

# Jelly page shortcuts browser check

Requires Cargo, Chrome and `playwright-cli`. Build from the checkout and start a
fresh throwaway daemon (substitute your scratch paths and port):

```sh
cargo build --locked --target-dir /tmp/jelly-keys-target
/tmp/jelly-keys-target/debug/jelly serve --port 17851 --home /tmp/jelly-keys-home
```

From a scratch directory, run with an absolute path to the script:

```sh
playwright-cli -s=keys open http://127.0.0.1:17851/
playwright-cli -s=keys run-code --filename=/path/to/jelly/tests/page-shortcuts.js
playwright-cli -s=keys close
```

The script creates and deletes only its own fixtures through the existing API.
It clicks into a jelly and checks that j, k, arrows, a and p still drive the page,
that a click back into the jelly that already has focus selects it again,
that Enter opens the jelly that has focus and that a and c still work afterwards
while focus stays in the stream frame, that comment mode begun in the stream and
followed by Enter is ended by Escape on the stream's own button so that c works
again after the full view closes, that repeated j inside one jelly walks the
stream and Enter then opens the jelly the ring marks, that c inside an unselected
jelly comments on that jelly, that Escape closes the dock from inside a jelly and
from the full view, that c turns comment mode on and a second c (now typed inside
the jelly) turns it off, that upper-case keys (Caps Lock) work on the page and
inside a jelly, that Ctrl, Alt and Meta combinations inside a jelly change nothing,
that letters and Escape typed into a jelly's own input stay there, that Enter on a
focused button, link or summary activates it without opening the full view, that a
jelly which calls `preventDefault()` on a key (j or Escape) keeps it, that Escape
with a comment pop open closes the pop and nothing else even when focus is outside
the pop's text box, and that a
and c work in the full view after a click into its frame. It also checks the Escape ordering: in
comment mode one Escape only ends comment mode, and the next closes the full view. Stop the daemon afterward. Existing homes retain their stored
jelly page; use a fresh home, or update it with
`jelly --url http://127.0.0.1:17851 reshape surface --view assets/surface.html`
from the checkout, merging these changes into any customized view first.

# Shared-file browser check

`cargo test` includes the shared-file browser regression. Chrome/Chromium is
required; set `JELLY_CHROME` to its executable if it is not installed in a usual
location. A missing browser fails the check rather than skipping it.

To run only the sharing checks:

```sh
cargo test --locked --bin jelly server::share_tests -- --nocapture
```

The test exports a jelly to a temporary HTML file, opens it through `file://`
and checks image pixels, CSS resources and keyframe edits, native CSSOM calls,
open/closed imperative and declarative shadow roots, unchanged inert templates
and custom-element registrations, dynamic objects using `jelly.asset` (implicit
dynamic object paths remain unchanged), static objects in open/closed declarative
roots (including rendered image dimensions), inert object markup, uploaded stylesheets
(including lowercase and mixed-case stylesheet relationship tokens, late link charset
selection, `jelly.asset` resolution and imports) and scripts, collection of
discarded roots with continued observation of live closed roots, captured
attention readers and read-only mutations.
It uses a temporary browser profile and a loopback test-result receiver, and
requires no daemon or real jelly home.

Separate Rust tests in `server::share_tests` assert against the exported HTML to
check HTML base eligibility and location-independent static object asset
resolution. These checks do not open the file in a browser and run with the same
command above.

# Stream header browser check

Requires Cargo, Chrome and `playwright-cli`. Build from the checkout and start a
fresh throwaway daemon (substitute your scratch paths and port):

```sh
cargo build --locked --target-dir /tmp/jelly-stream-target
/tmp/jelly-stream-target/debug/jelly serve --port 17850 --home /tmp/jelly-stream-home
```

From a scratch directory, run with an absolute path to the script:

```sh
playwright-cli -s=stream open http://127.0.0.1:17850/
playwright-cli -s=stream run-code --filename=/path/to/jelly/tests/stream-header.js
playwright-cli -s=stream close
```

The script creates and deletes only its own fixtures through the existing API.
It checks the actual served stream in a touch-enabled mobile context at 390px,
with device-width and scale assertions, and at 320, 900, 901 and 1280px. Normal,
long and unbroken titles and metadata, status badges, activity, and expanded
Comment labels must fit without overlap or horizontal overflow. It checks action
hit targets, responsive visibility, desktop reveal, forward/reverse tab order,
visible keyboard focus, Comment and iframe Escape, c, j/k, Enter, Open, Back,
Pin persistence, and cancelled/confirmed Remove. Project and All identification
are checked at 390 and 1280px.

It returns measurements and saves `stream-390.png`, `stream-390-expanded.png` and
`stream-1280.png` in the scratch directory. Inspect these images for readability.
Run `full-view-header.js` against the same daemon to check full-view behavior.
Stop the daemon afterward with Ctrl-C. Existing homes retain their stored jelly
page; use a fresh home to test the newly built bundled view. To explicitly update
a stored page, use `jelly --url http://127.0.0.1:17850 reshape surface --view assets/surface.html`
from the checkout, merging these changes into any customized view first.
