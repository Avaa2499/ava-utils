# ava-utils

A growing suite of small, independent Rust modules, optimised around the user's Linux machine. Each module does one bounded job and publishes its own inputs and outputs. Modules work together through shared topics and calls, never through direct dependencies. New things get their own module, never tacked onto an existing one. The suite is designed to grow indefinitely.

**Status:** building. `ava-utils-config`, the `au` CLI, `ava-utils-file-indexer`, `ava-utils-file-searcher` and `ava-utils-file-watcher` are done. The message format is frozen, the hardening items are built, and `au` is on PATH. Open work, including the review follow-ups and the free-model subagent setup, is in `todo.txt`.

## Locked decisions

- **Language:** Rust.
- **NO SINGLE POINTS OF FAILURE.** No running ava-utils process that other modules need. systemd and the filesystem are the accepted floor; replace them where it's genuinely better and worth redoing.
- **Platform:** Linux for now. Other OSes later. Needs systemd 254 or newer (`RestartSteps=` and `RestartMaxDelaySec=`); only 259 is verified.
- **Dependencies:** no module depends on another module. External crates and shared library crates are allowed.
- **Coupling:** no module depends on another module at compile time. At runtime a module reaches another by module name, but the name lives only in the manifest, never in code. Renaming or splitting a module means a find-and-replace across the manifests that name it. No module code changes and nothing is rebuilt.
- **Communication:** topics (events) and calls (request/response), declared in each module's manifest.
- **Data path:** peer to peer. Never a central broker.
- **Cycles:** loops made only of calls are banned (they can deadlock), checked at install time. Loops that include an event are allowed only if every module in the loop names the others in its manifest. See Cycles.
- **Lifecycle:** systemd user units with socket activation.
- **Isolation:** one OS process per module.
- **Repo:** one Cargo workspace.
- **Trust:** any ava-utils module can talk to any other.

## Architecture

A distributed mesh of concept-bound modules. Data flows directly between modules. If one module fails, nothing else fails.

## Standards

Every module follows these. No exceptions, ever.

1. **Naming:** `ava-utils-*`, e.g. `ava-utils-video-playback`: input a video file or stream, and it plays it. A module is named for what it works on, so `file-indexer`, `file-searcher`, `file-watcher` (a web search module would be `web-searcher`). The CLI crate is `ava-utils-cli`; its program is `au`, used as `au file-searcher find "*.rs"`.
2. **Communication:** on startup, a module subscribes to the endpoints it wants. If one is down, it retries after 2s, then 4s, doubling each time. At 2 minutes it flags a hard error and checks once every 5 minutes, so a module can be replaced live without restarting the stack.
3. **Request format:** the envelope (the boxes every message carries) is defined in `ava-utils-config`. Its details get decided when that crate is built.
4. **Library:** every module is a library crate (does all the real work, testable without starting anything) plus a small program that starts up, listens for messages and hands them to the library.
5. **Sockets only:** modules only talk over sockets. A module never loads another module's library directly.
6. **GUI:** a normal module, same rules as every other.

## Messaging

- Our own protocol over Unix sockets. No Varlink.
- `ava-utils-config` is the one shared crate: the talking code (connect, encode, send, wait for reply, retry), every message definition, file-location messages, and command-line flag reading. It never runs on its own, so it can't go down.
- Messages carry data in named boxes (e.g. `paths`, `total size`). A message always uses the same box names. Boxes can be optional. New boxes can be added later; old modules ignore boxes they don't know.
- Every message and its boxes are defined once, as Rust code, in `ava-utils-config`. Renaming or removing a box is allowed: the compiler shows every module using it, you fix them, and they're reinstalled together.
- Replies normally take a few milliseconds. No reply within 5s = that module is down.
- Events can be batched: one "50,000 files added" message with the list, not 50,000 messages.
- Producers never block. Each subscriber has a size-limited queue; if it overflows, that subscriber is disconnected, reconnects and gets the current state.
- When a subscriber connects, the producer immediately sends the topic's current state. Nobody needs a call to resync.
- Topic and call names: no fixed scheme, named to fit what they are.
- Messages are binary, encoded with MessagePack. It's hidden inside `ava-utils-config`, so it can be swapped for our own format later without touching any module. Each message carries its box names, not just the values, so updating one module never breaks the others. The `au` CLI is the parser: it shows messages as readable text, with a flag to show the raw binary instead.
- Codecs: `ava-utils-codecs` is a shared crate, not a module. It uses the codec libraries already installed on the system (libavcodec, libdav1d, libvpx, libopus), so the heavy code exists once. Video playback, audio playback and convert-file (e.g. .mkv → .mp4) build it in.
- Never two modules doing the same thing. This is a design rule, not an install check. Install only checks loops, manifests and tests.

## Message format (frozen)

Frozen once three modules worked together. The exact wire bytes of every message and the envelope are saved in `crates/ava-utils-config/tests/golden/`, and a test fails if they change.

- **Free, no ceremony:** add an optional box (`#[serde(default)]`), add a message or topic, add an enum variant. Old modules ignore unknown boxes and decode unknown enum variants as `Unknown`.
- **A deliberate format change:** rename or remove a box, change a box's type, make an optional box required, change what a box means. The golden test fails; review the diff, then run `AU_BLESS=1 cargo test -p ava-utils-config --test golden` to accept it, fix every module the compiler points at, and reinstall them together.
- **Enums on the wire** must have a `#[serde(other)] Unknown` variant, and callers must handle it (refuse, or treat as unsupported).
- **Errors** use the standard codes in `ava-utils-config` (`error::codes`): `unavailable`, `unknown-call`, `bad-request`, `not-a-call`, `failed`. Modules map their errors with `Envelope::from_error`.
- **Paths are UTF-8 strings.** A name that is not valid UTF-8 is skipped and counted, never silently lost: `au file-indexer status` and `au file-watcher` show the count.

## Long jobs

- Long work replies instantly with "started" plus a job ID (a random unique label, e.g. `7f3a9c`).
- Progress and ETA are only sent when asked for.
- **Force cancel:** the job stops instantly.
- **Safe cancel:** the job saves where it is and stops. It can pick up from there later with no lost work.

## Manifest

- Every module comes with a manifest listing the names of the messages it sends, listens to and answers. (The boxes inside each message are defined in `ava-utils-config`.)
- Install reads the manifests to check for loops.
- Each manifest binds every call a module makes and every topic it listens to onto a module name (e.g. `file-lookup` → `file-indexer`). A module reads its manifest to find the socket to connect to. The code only knows the message, never the module name.
- Install refuses a module with a missing manifest, or whose messages don't match it.
- Stored in `~/.config/ava-utils/`.
- It also lists the module's CLI commands (name + one-line help), so `au help` and tab completion work without starting the module.

## Cycles

Two different dangers, two different rules. Checked at install time from the manifests; nothing stays running.

- **Deadlock (loops made only of calls):** banned at any length (A → B → A, A → B → C → A). Only calls wait for an answer, so only a loop of calls can deadlock. The one exception is two modules that name each other in `loop_partners` (A ⇄ B). The 5s reply cutoff still applies to that pair.
- **Loops that include an event:** events never wait (producers never block, queues are bounded), so these cannot deadlock. They are allowed only if **every module in the loop names every other module in it** in its `loop_partners`, whatever the loop's length. A loop nobody named is refused. Example: `gui` calls `file-watcher`, `file-watcher` sends events to `file-indexer`, `file-indexer` sends events to `gui`: all three name the other two.
- Naming is always mutual. If only one side names the other, install refuses it.
- **Event storms (A's event makes B send one that makes A send one...):** every event carries an optional `hops` box. A module that sends an event because of one it received must use `Publisher::publish_caused_by(&received, ...)`, which adds one hop. Past 16 hops the event is dropped, and subscribers also skip events that arrive past 16. A module that publishes through plain `publish` starts again at 0, so that is only for events with no cause. Bounded subscriber queues still disconnect an overloaded subscriber, so a storm slows the modules in the loop and nothing else.

## Launching

- Each module's socket: `$XDG_RUNTIME_DIR/ava-utils/<module>.sock`, owned by a systemd socket unit.
- Template units: `ava-utils@.socket` + `ava-utils@.service`, e.g. `ava-utils@video-playback.service`.
- Connecting to an endpoint starts its module. Nothing starts until something connects.
- A module that only listens (e.g. the indexer) starts when something calls it, or when you start it by hand. On startup it subscribes to what it listens to, which starts those modules. Events it missed while stopped are covered by the current state each producer sends on connect.
- Nothing starts at login, and nothing runs while you're logged out. Decided: this stays until most modules are built and working; then revisit enabling only the listening modules (file-watcher, file-indexer) at login. Until then the watcher does not watch until something starts it, and the indexer catches up with a rescan when it connects.
- Once started, a module keeps running until you log out or shut down.
- Verified by a spike (systemd 259): the first connection starts the service; `kill -9` and `abort()` both bring it back within 2s (`Restart=always`); a client that connects while it restarts is queued by the socket and answered by the new process. A client connected at the moment of the crash gets a dropped connection and must retry.
- A crashed module restarts on its own, without waiting for a new connection. "Nothing starts until something connects" applies to the first start only.
- The module gets its socket from systemd as fd 3 (`LISTEN_FDS`), so it never binds or creates the socket itself.

## Module storage

- Each module keeps what it must remember after a restart in its own folder, `~/.local/state/ava-utils/<module>/`. Nothing scattered across the system.
- A module never touches another module's folder.
- Big working files (e.g. 40,000 files unpacked from a zip) go in `~/.cache/ava-utils/<module>/`. The module deletes them when the job finishes.
- A shared cache crate for big working files gets built only if one is actually needed.

## The `au` CLI

- Installed with `just install` (builds it, copies it to `~/.local/bin/au` and writes the bash completion to `~/.local/share/bash-completion/completions/au`; `au install` cannot install itself). Re-run after changing the CLI.
- One command for everything: install, remove, cycle check, and using modules as `au <module-suffix> <arguments>`.
- It exits when done, so nothing depends on it running.
- Reading command-line flags lives in `ava-utils-config`, not in a module.
- Every module answers `run-command` with no arguments by replying with a short "I'm up" text. Install uses this to start the module for the crash test.
- `au <module> <args>`: the CLI sends the raw arguments to the module in one standard "run this command" message. The module parses them (with the flag code in `ava-utils-config`) and replies with text to print. The CLI knows nothing about any specific module's arguments.
- Updating a module replaces it and restarts only that module. Everything else keeps running.
- Removing a module: anything using it follows standard 2's retry rules and carries on doing what it can.

## Fault containment

A crash, hang or bad input in one module never takes down another. A failed operation still fails; the caller gets a typed error and carries on in a reduced way.

| Failure | Handling |
|---|---|
| Crash | Restart forever, never give up: `Restart=always`, `RestartSec=2s`, `RestartSteps=7`, `RestartMaxDelaySec=5min` (≈ doubling, 2s → 5 min). Sockets: `TriggerLimitBurst=0` |
| Hang | `WatchdogSec=60s` + `NotifyAccess=main`. A module pings systemd every 20s, but only after its own `run-command` probe (sent to its own socket) succeeds, so a stuck accept loop or frozen process gets killed and restarted. Plus the 5s reply cutoff. Verified live: a `SIGSTOP`-frozen module was restarted after ~59s |
| Bad input | Before handling a request, the server writes it to `inflight.txt` in the module's state folder. A request still in flight when the module restarts gets a strike (a panic in its handler also counts); a normal finish resets it, so it means 3 crashes in a row. At 3 the request is refused (`refused` error) until `au <module> quarantine clear <text or all>`; `quarantine list` shows them. Probes, `job-progress` and `job-cancel` are never tracked. A scan job that was running at 3 crashes is given up on (state Failed) |
| Runaway memory | `MemoryMax=8G` in the template (a scan of ~280k entries peaks near 520 MiB). A module that needs more, e.g. a huge watcher journal, raises it with `systemctl --user edit ava-utils@<module>.service`; `au file-watcher journal` warns when its limit cannot fit |
| Lost state | Anything needed to resume lives on disk, not only in memory |
| Double-apply on retry | A retry that reuses its request id (`client::call_retrying`) gets the first reply replayed from a per-module cache (1024 replies, 10 minutes, memory only, successful replies only). After a crash the cache is empty, so operations must also be idempotent |
| Missed events while down | Producer sends current state when the subscriber reconnects. For `files-changed` the state and every event also carry the watcher's `epoch` (new on every watcher start), a batch counter `seq`, `first_seq` (the oldest batch the journal still holds in full) and the watched `roots`. The indexer saves the position it reached. On connect it rescans the watched roots as a normal job if the epoch changed or the journal no longer reaches back to its position; it skips the rescan when the journal covers the gap. A skipped number in the live stream makes it reconnect. The saved position only moves forward once the rescan finishes |

All directives verified present in systemd 259 on this machine. `RestartSteps=` and `RestartMaxDelaySec=` need systemd 254 or newer, so older distributions (e.g. RHEL 9, Ubuntu 22.04) are not supported.

## Rules for every module

1. Pass references (paths), never bulk bytes over a socket. Handing over file handles gets built when a module first needs it.
2. Every call has a deadline; every caller handles `Unavailable`.
3. Every module is heavily tested. Install runs its tests automatically, including a `kill -9` crash test: killed mid-job, it comes back and nothing else is affected.
4. tokio only in each module's runnable program. Libraries stay plain Rust.
5. Logs go to journald. Every line is tagged with the module name and the job/message ID.

## Git

- Commit after every small change, with a clear message. Never batch changes.
- **HARD RULE: push to GitHub (`origin`) after every commit.** Never leave commits only local. Parallel subagents work from what is on GitHub. Pull with rebase before pushing; if a push is rejected, rebase and push again.
- Never leave branches open long. Keep them short-lived: merge to `main` quickly, then delete the branch.

## Build and workflow

- Subagents use Haiku by default. If something breaks later, revisit it with Sonnet.
- Linker: mold, set in `~/.cargo/config.toml` (user level, so every worktree and subagent gets it).
- Compiler cache: sccache (the Rust equivalent of ccache), also user level. Each parallel subagent builds in its own worktree, and sccache shares the compiled dependencies between them.
- Tasks: a `justfile` with `check`, `test`, `clippy` and `fmt`, added when the Cargo workspace is created.

## Traps

1. `catch_unwind` is not isolation. It misses aborts, stack overflows, C segfaults, OOM and deadlocks. The process is the boundary.
2. Rust `.so` plugins break: no stable ABI. Processes + a wire protocol avoid this.
3. Untagged wire formats (bincode-style) break when a field is added.
4. Never split a realtime pipeline across processes: decode and audio output stay in one module.
5. One structured log stream for everything, tagged with module and request ID.

## Open questions or todo

See `todo.txt`.

## Example modules

Examples only, not a roadmap: audio playback, video playback, compression, file indexing, file watcher, media probe, thumbnails.

## Build order

1. Topics and calls for the first three modules; check them against the cycle rule. (~1 hour)
2. `ava-utils-config` + manifest format + `au` CLI with the cycle check. (~a weekend)
3. First module: picked once the whole architecture is planned.
4. Two more modules of different kinds.
5. Freeze the message format only after three modules work.

## First modules: messages

Draft. Names are working names; the boxes get defined in `ava-utils-config`.

| Module | Answers | Sends (topic) | Listens to |
|---|---|---|---|
| file-indexer | `run-command`, `index-lookup`, `index-scan` (long job), `job-progress`, `job-cancel` | none | `files-changed` |
| file-searcher | `run-command`, `search` | none | none |
| file-watcher | `run-command`, `watch-set-roots` | `files-changed` (batched, current state on subscribe) | none |
| cli | none | none | none |

Calls made: cli → file-indexer, file-searcher, file-watcher (`run-command`); file-searcher → file-indexer (`index-lookup`). Topic flow: file-watcher → file-indexer.

Loop check: the graph is cli → {file-indexer, file-searcher, file-watcher}, file-searcher → file-indexer, file-watcher → file-indexer. No loops, so no loop exceptions are needed.

Decisions behind it:
- The file-indexer owns the index; the file-searcher never reads it directly, it calls `index-lookup`.
- The file-watcher owns its list of roots (set by the cli), so the file-indexer never calls the file-watcher. That would be a loop (file-watcher → file-indexer events, file-indexer → file-watcher call).
- The file-indexer applies exclusion rules to what the file-watcher reports, so the file-watcher watches everything under its roots.
- No `index-changed` topic yet: nothing listens to it. Add it when something does.
- `run-command`, `job-progress` and `job-cancel` are the standard messages every module answers (accepted names). The CLI picks the module by name, so they never clash.


TODO MODULES:

gui
audio playback
video playback
compression
file indexing
file watcher
file searcher
convert file

SHARED CRATES (code built into modules, never run on their own):

ava-utils-config
ava-utils-codecs
