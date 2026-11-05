# Rex view navigation

The Rex bridge lets one directional shortcut move through Helix views and then into another Rex pane. Helix keeps its editing mode, selections, and unsaved text when the bridge changes views. Native Helix navigation commands keep their existing behavior.

The bridge starts when Helix inherits `REX_SESSION` and `REX_BLOCK`. It creates a socket for that process under Helix's cache directory, in `rex/<pid>.sock`. The directory belongs to the current OS user and has mode 0700; sockets have mode 0600. A capability handshake identifies the protocol, process instance, session, and block.

## Build and check

Build with the repository's Rust toolchain and locked dependencies. Grammar downloads run separately:

```sh
HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo build --release --locked -p helix-term --bin hx
HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo test --locked -p helix-term --lib rex::tests
HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo test --locked -p helix-term --features integration --test integration
python3 contrib/test_rex_navigation.py --binary target/release/hx
```

The terminal check uses temporary files and a separate editor process. It covers nested views of one file, four directions, pending prompts and key sequences, a lost reply after a successful move, an expired request, Insert mode, and endpoint cleanup. It saves its terminal transcript beside those temporary files.

## Request a move

The Rex Lua action checks `hx --rex-navigate probe` before sending a move. A helper from an older build fails that read-only check and leaves Rex navigation available. It runs the move helper on the host that owns the Helix pane:

```text
hx --rex-navigate PID DIRECTION SESSION BLOCK
```

Directions are `left`, `right`, `up`, and `down`. The helper prints one JSON response. `before` and `after` identify the editor views when a layout decision ran.

| Outcome | Effect |
|---|---|
| `moved` | Helix focused another internal view. Finish the key without moving Rex. |
| `edge` | Helix has no view in that direction. Perform Rex's existing pane-focus action. |
| `blocked` | A prompt, picker, completion, count, or pending editor key sequence prevents an internal move. Preserve that input and perform Rex's pane-focus action. |
| `stale` | Helix received terminal focus loss before applying the move. Finish without an outer move from the obsolete context. |
| `not_applied` | Cancellation or expiry prevents this request from moving Helix later. Rex may perform the outer action. |
| `unavailable` | The capability check failed before sending a move. Use normal Rex navigation. |
| `unknown` | A request may have run, but its result could not be recovered. Finish this key without another move; a later key can try again. |

Each move has a request ID and a 150 ms lifetime. The editor callback and cancellation share a lock. Cancellation returns a completed move's existing result, or prevents a pending move from running. After losing a reply, the helper resolves that same ID instead of issuing another move. Requests execute on the editor thread through the existing job queue.

Normal exit removes the process socket. Starting another integrated editor removes sockets whose processes have exited. Other live editor sockets remain available.

## Rex dispatch

The companion dotfiles module registers the existing directional aliases and queues the original `pane.focus` client action for an outer handoff. It preserves the action's wrapping argument. Resize mode and floating-terminal bindings remain in the existing Rex configuration.

For a pane on another host, the originating Rex action calls `helix_navigation_request` on that host's Rex server. That action inspects the foreground process and runs its local helper. It returns the result to the originating client, which owns the outer pane-focus action. Both hosts need the matching Lua module and integrated executable.

## Installed runtime pairs

An explicit `HELIX_RUNTIME` takes precedence over the user-config runtime and Cargo's development path. The installer launcher selects one immutable build directory and runs its binary with that directory's runtime. Changing the selected build affects new processes; existing processes keep their selected runtime. The global user-config runtime link can remain available to older editors.

## Numbered buffers and editor helpers

The companion Rex configuration routes `Cmd+1` through `Cmd+9` to Helix's bufferline positions while Helix has focus. Outside Helix, Rex keeps numbered terminal-tab selection. `hx --rex-navigate probe-buffer` checks the helper; the socket handshake's `buffer_navigation` flag checks the running editor. An older editor retains view navigation but needs a restart for numbered buffers.

A buffer request uses `buffer:N` in place of a direction. It follows the same request-ID, cancellation, and expiry rules. A missing buffer or pending prompt stays inside Helix. The request never falls back to selecting a Rex tab after identifying Helix.

`:rex-tool` passes parsed arguments to the dotfiles helper without a shell expansion step. The helper launches a new Rex tool pane with the originating session/block and an explicit working directory. Project tools such as RSpec and Rails select their environment with `mise exec`; editor helpers run directly without resolving unrelated global tools. Filenames and prompts remain separate arguments, including quotes and spaces.

`:rex-pipe` sends one selection to a helper. Before applying output, it checks the document revision, view, selection, mode, terminal focus, and pending input. Changing or closing that context discards the delayed result. The edit and its selection enter the document history together. `--clipboard` copies an accepted result after those checks. `HELIX_REX_HELPERS` can select a helper script when testing a staged dotfiles checkout.

```sh
python3 contrib/test_rex_helpers.py --binary target/release/hx --dotfiles /path/to/dotfiles
```

This check covers numbered buffers, literal arguments, delayed-result guards, initializer placeholders, and both YARD comment paths with the actual Helix configuration. It uses a private terminal and a file-backed clipboard, so the test does not take desktop focus or replace the system clipboard.
