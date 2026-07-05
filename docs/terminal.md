# Terminal forwarder

Status: implemented

## Context

Getting a shell on the machine running ddisplay-server today means setting up
sshd, keys and a second port. The server already accepts websocket clients
that can inject arbitrary input into the session, so a remote shell adds no
privilege the connection doesn't already grant. A terminal channel over the
existing connection gives "ssh but zero setup", and it must be usable
alongside the video stream: a shell window on top of the running desktop, in
the native client.

## Decision

F4 in the native client toggles a terminal window rendered with egui inside
the client window, alongside the video stream. The terminal attaches to a
fresh PTY on the server over the same websocket connection the video uses.
The remote shell generates the byte stream; the client interprets it with the
`vt100` crate and draws the resulting cell grid. One terminal per connection.

Wire protocol, new 0x30 block:

- `0x30 TermOpen` (client to server): JSON `{cols, rows, term}`. Opens a PTY
  running `$SHELL` (fallback `/bin/sh`) with `TERM` set from the client. A
  TermOpen while the connection's shell is still alive is ignored (the dying
  session's trailing TermData/TermExit would corrupt the new one); after a
  TermExit the next open starts a fresh shell.
- `0x31 TermData` (bidirectional): raw bytes, keystrokes up, PTY output down.
- `0x32 TermResize` (client to server): `[cols: u16 LE][rows: u16 LE]`.
- `0x33 TermExit` (server to client): `[exit_code: u8]`. Sent when the child
  exits or the open is refused (readonly connection).

Server: `terminal.rs` opens the PTY pair with `posix_openpt` and spawns the
shell with the slave as its controlling tty. Two plain threads pump the
master fd: a writer draining an mpsc of Data/Resize messages, a reader
forwarding output into the connection's existing per-client direct channel
(backpressured by `blocking_send`). The session is owned by the websocket
connection: when the connection drops, the input channel closes and the
writer sends the child SIGHUP (ssh semantics: a process that ignores SIGHUP
keeps running). The reader reaps the child and reports the exit code.
`TermOpen` is refused on readonly connections. Terminal messages are handled
in the connection task and never reach the input injector.

Client: `term.rs` holds a `vt100::Parser` fed by incoming TermData and draws
the screen grid as one `LayoutJob` per row (monospace, per-cell fg/bg from
vt100) inside a movable, resizable egui window; the desktop stays visible and
interactive around it. Resizing the egui window recomputes cols/rows from the
glyph cell size and sends TermResize. Keyboard input goes to the terminal
only while its widget has egui focus (click to focus): egui text events are
sent verbatim, control keys are translated (Ctrl-letter, Enter, Backspace,
arrows honouring application cursor mode, Home/End/PgUp/PgDn/Delete, Esc,
Tab). Focused-terminal keys are consumed by egui and never forwarded as
remote desktop input; clicks and keys outside the terminal window keep going
to the desktop. Showing the window with no live shell (first F4, or reopening
after an exit) sends TermOpen with the current grid size and
TERM=xterm-256color. Closing the window (F4) hides it but keeps the shell
running; the session dies with the shell or the connection.

## Alternatives

- CLI passthrough mode (`--term` attaching the invoking console): simplest
  possible client, but it cannot show the shell alongside the video stream,
  which is a hard requirement. The protocol still supports such a mode later.
- Server-side emulation with screen-state sync (mosh style): resilient to
  drops but a protocol an order of magnitude bigger. Rejected, YAGNI.
- `alacritty_terminal` for emulation: full-featured but heavy; `vt100` is a
  small parser-to-grid crate that fits an egui redraw loop directly.
- `portable-pty` on the server: openpty is ~100 lines with `libc`, which the
  server already depends on. Rejected to keep the dep tree flat.
- Dedicated TCP port or /term ws route: reusing the existing connection keeps
  one endpoint, one firewall rule, and the existing readonly gate.

## Consequences

- Anyone who can reach the websocket gets a shell as the server user. This is
  the same trust level as the existing unauthenticated input injection, but
  it makes the implication explicit; when authentication lands it must cover
  `TermOpen`.
- Terminal output shares the per-client direct channel with RTT pong echoes
  and is drained before video frames (biased select). A flood of PTY output
  can delay frames and occasionally drop a ping; accepted, bounded by the
  8-frame broadcast buffer and IDR recovery.
- A child that ignores SIGHUP outlives the connection and its reader thread
  blocks until the child exits. Bounded by user behaviour, accepted.
- The web client can later reuse the same messages with xterm.js unchanged.
- The client gains a `vt100` dependency.

## Acceptance

- F4 opens a shell window over the streaming desktop; both stay usable: keys
  typed with the terminal focused go to the shell, clicks and keys outside it
  go to the remote desktop.
- Interactive use works: line editing, colors, Ctrl-C reaching the remote
  job, and a fullscreen program (htop, vim) drawing correctly after the
  terminal window is resized.
- Shell exit shows an exited notice; F4 (close, reopen) starts a fresh shell.
- Killing the client mid-session leaves no zombie shell on the server
  (SIGHUP delivered, child reaped).
- Readonly connections (`?readonly=true`) cannot open a terminal.
- `cargo build` for server and client passes on Linux; the client also still
  builds for Windows (CI).
