# Editor control and external programs

Helix exposes local editor operations through an opt-in Unix socket. A terminal adapter chooses which editor to contact and handles terminal panes, tabs, and tool placement. The editor does not interpret terminal session IDs or launch a particular terminal.

For example, an adapter asks Helix to move left from its right-hand view. Helix focuses the left-hand view and returns `moved`. A second request returns `edge`; the adapter can then focus a terminal pane to the left. Internal navigation preserves Normal, Insert, or Select mode and each document's selections. Native Helix focus commands keep their existing behavior.

## Enable and discover an editor

Set `HELIX_CONTROL_CONTEXT` to an opaque string before starting Helix. The adapter should choose a value that identifies the originating pane and replace inherited context when starting in a different pane. Without this variable, Helix does not create a control endpoint.

```sh
HELIX_CONTROL_CONTEXT=example-pane hx file.txt
hx --control probe
hx --control locate example-pane
hx --control 12345 left example-pane
hx --control 12345 buffer:2 example-pane
hx --control 12345 split:right example-pane
```

`locate` returns matching live handshakes with `pid`, `protocol`, `instance`, `context`, `buffer_navigation`, and `split_creation`. Adapters must identify the foreground editor when several instances share a pane. `probe-buffer` and `probe-split` report executable capabilities; the handshake describes the running process.

Sockets live in `control/<pid>.sock` under Helix's cache directory. The directory belongs to the current OS user and has mode 0700; sockets have mode 0600. The protocol is for local processes with that user's access. Normal exit removes the endpoint. Startup removes endpoints owned by the same user whose processes have exited.

## Operations and results

Navigation accepts `left`, `right`, `up`, and `down`. Numbered buffers use `buffer:N`, with positions starting at one and matching the bufferline. Split creation accepts `split:right` and `split:down`; it uses Helix's native same-buffer split operation and enters Normal mode. An internal split does not create a terminal pane.

| Outcome | Adapter behavior |
| --- | --- |
| `moved` | Finish this key; Helix applied the operation. |
| `edge` | A navigation request found no adjacent view; the adapter may move terminal focus. |
| `blocked` | A prompt, picker, completion, count, or pending editor key prevents the operation. Preserve that input. A navigation adapter may move terminal focus. |
| `stale` | Terminal focus changed; finish without an outer action. |
| `not_applied` | Cancellation or expiry prevents a delayed editor operation. A navigation adapter may move terminal focus. |
| `unavailable` | No matching endpoint or capability; the adapter decides the application fallback. |
| `unknown` | The result could not be resolved. Finish without another action, since the editor may already have applied it. |

Before moving outer focus, an adapter must recheck the originating pane and foreground process. Buffer and split requests targeting Helix must not fall back to terminal tabs or splits after a blocked or unresolved result.

Requests carry an instance, ID, and a deadline capped at 150 ms. The editor callback and cancellation share a lock. A repeated ID returns the committed result. After losing a reply, the CLI resolves that same ID through cancellation; it does not issue another move. Requests execute through the existing editor job queue.

## Programs with structured arguments

`:run-program -- PROGRAM ARGS...` starts a program asynchronously in the editor's working directory, without reparsing arguments through a shell. Put program flags after `--` and quote arguments containing spaces. A leading `~` in the program path expands to the home directory.

```text
:run-program -- /path/to/tool-launcher files "a file.txt"
:pipe-program -- /path/to/formatter --language ruby
:pipe-program --clipboard -- /path/to/extract-errors
```

`:pipe-program` sends one selection to the program and replaces it with successful UTF-8 output. Before applying output, Helix checks the document revision, view, selection, mode, terminal focus, overlays, and pending input. A changed context discards the result. The edit and resulting selection enter document history together. `--clipboard` copies the accepted output after these checks; a clipboard failure preserves the document.

Helix's existing shell commands remain available. These commands provide explicit argv and guarded asynchronous replacement for external adapters and helpers. Terminal selection, tool naming, working-directory choice, and focus restoration belong to those programs.

## Build and focused checks

```sh
HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo build --release --locked -p helix-term --bin hx
HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo test --locked -p helix-term --lib control::tests
python3 contrib/test_control_navigation.py --binary target/release/hx
python3 contrib/test_control_splits.py --binary target/release/hx
python3 contrib/test_control_helpers.py --binary target/release/hx
```

The terminal checks use private pseudo-terminals and temporary files. They cover nested views, four directions, mode preservation, pending input, discovery, wrong context, lost replies, expiry, duplicate splits, literal arguments, stale replacements, and a temporary clipboard provider.

An explicit `HELIX_RUNTIME` takes precedence over the user-config runtime and Cargo's development path. An installer can select an immutable binary/runtime pair for new processes while existing editors keep their selected runtime.
