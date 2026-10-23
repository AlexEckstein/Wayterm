# SFTP Sidebar

A remote file browser for each SSH tab, shown in the side dock's SFTP
panel (doc 01 §Side dock): navigate, upload, download, delete, rename,
create folders, drag files in, and follow the shell's working directory.

Code: `src/sftp/` (the tokio half: task, commands, shell-directory probe),
`src/ui/panels/sftp_sidebar.rs` (the GTK half), `src/ui/panels/sftp_sidebar/model.rs`
(its display-free model).

## Layout

Each SSH tab owns its sidebar, but the tab's page holds only the
terminal: the side dock's SFTP panel hosts the current tab's sidebar and
swaps it as the notebook switches tabs. Tabs without one (local, serial,
RDP, VNC, Home) get a placeholder. A redial replaces the page's sidebar,
ending the old SFTP task, and the panel hosts the new one.

The sidebar, top to bottom:

- a path bar (Enter navigates);
- a toolbar: Upload, Download, Delete, Rename, Refresh, New folder;
- an inline error line;
- the listing: name, size, modified (UTC), permissions; directories first,
  then files, case-insensitive; a `..` row except at `/`;
- a progress bar with Cancel and a status line;
- the "Follow terminal directory" check box (§Following the shell).

`toggle_sftp` (Ctrl+Shift+B) switches the dock to the SFTP panel, or back
to the panel it came from; a panel hidden by `toggle_session_tree` comes
back on SFTP. While focus is in the panel (not its path bar), the
`sftp_*` shortcuts press the toolbar's Upload, Download, Rename and Delete
buttons, enabled or not as clicks are (doc 11 §Handlers). Download and
Upload use file choosers; Rename and New folder ask for a name; Delete
asks for confirmation.

## Connection model

The sidebar runs over the tab's **shared** `SshConnection` (doc 04
§Connection supervisor): no second process, no second login. Attaching
spawns one SFTP task per sidebar; until then the sidebar is empty with its
toolbar disabled. The task opens a session channel with the `sftp`
subsystem and turns it into a `russh_sftp` session on the first command.
Dropping the last sidebar handle ends the task; the connection stays with
the supervisor.

When it attaches depends on the `sftp_on_connect` setting (on by default,
doc 13 §Settings rows):

- **On:** the sidebar attaches when its tab connects, and a connect the
  user started on the current tab brings the SFTP panel forward, unless
  the user has hidden the dock's panel; a reconnect never switches the
  dock.
- **Off:** the connect flow only hands the sidebar the connection; the
  task starts the first time the SFTP panel has that tab's sidebar in
  view, so a user who never looks never opens an SFTP channel. A sidebar
  already shown when the connect lands attaches at once.

## Commands

```rust
pub enum SftpCommand {
    Cd { path: String, reply: oneshot::Sender<Result<Vec<DirEntry>, String>> },
    Download { transfer_id: u64, remote_path: String, local_path: String,
               progress: mpsc::Sender<Progress>, reply: oneshot::Sender<Result<u64, String>> },
    Upload   { transfer_id: u64, local_path: String, remote_path: String,
               progress: mpsc::Sender<Progress>, reply: oneshot::Sender<Result<u64, String>> },
    Delete { path: String, reply: oneshot::Sender<Result<(), String>> },
    Rename { from: String, to: String, reply: oneshot::Sender<Result<(), String>> },
    Mkdir  { path: String, reply: oneshot::Sender<Result<(), String>> },
    Cancel { transfer_id: u64 },
}
```

The enum is a module boundary (AGENTS.md).

- GTK code never awaits on the send: it uses the never-blocking
  `IdleSender` (doc 02 §Backpressure) and awaits each oneshot reply in a
  main-context future.
- Every command but `Cancel` is answered exactly once — with `Err` when
  the session is dead. Any failure drops the cached SFTP session and the
  next command reopens it, so a server closing the subsystem shows up as
  an error ("SFTP session ended — reconnect?") and recovers.
- `Cd` doubles as the listing command. There is no `getcwd`, so the path
  bar shows the requested path; browsing starts at `.`, the SFTP home,
  unless the shell's directory is known by then.
- `Delete` stats first: russh-sftp 3 splits removal into file and
  directory calls (directories must be empty).

russh-sftp 3.0 anchors: `SftpSession::new(channel.into_stream())`;
`read_dir` is a sync iterator; `remove_file`/`remove_dir`; async
`File::close`.

## Transfers

Downloads and uploads run as their own tasks, registered by a
`transfer_id` the client pre-generates, so `Cancel` and browsing stay
responsive mid-transfer. They copy in 32 KiB chunks; progress samples go
out with `try_send` (a stale-but-moving bar is fine, a stalled transfer is
not). The bar shows whichever transfer updated last and Cancel targets the
newest. An upload's final flush and close must succeed or the transfer
fails, since the remote file may be incomplete. Every successful operation
refreshes the listing.

## Following the shell

While "Follow terminal directory" is ticked (on by default, per tab, kept
across a redial), the sidebar shows the directory the tab's shell is in.
It learns that two ways:

- **OSC 7.** A shell that reports its directory (`file://host/path`,
  percent-encoded) makes VTE emit `current-directory-uri-changed`; the tab
  hands the URI to a `DirectoryFollower`. The tab connects to the VTE
  widget directly, as it does for the window title, so `TerminalBackend`
  and `TerminalEvent` are unchanged. The first report pins the host:
  reports naming another host come from a nested `ssh` inside the
  session, whose paths are not on this connection. This is a heuristic:
  if the session's own shell never reports, the first nested host that
  does is pinned.
- **Probing.** Many shells send nothing over SSH: bash by default,
  oh-my-zsh (it turns OSC 7 off when `$SSH_TTY` is set), and any shell
  inside tmux, which keeps OSC 7 for itself. So after each command (Enter,
  then 300 ms without terminal output) the sidebar asks the remote with a
  `ShellCwdProbe` (`src/sftp/shell_cwd.rs`): one `sh` on an exec channel
  of the shared connection, its script on stdin so the login shell never
  parses it. A tmux tab asks
  `tmux display-message -p -t '=<name>:' '#{pane_current_path}'`. A plain
  tab walks `/proc` from the probe up to this connection's SSH server
  process, takes its one child on a terminal that is not running tmux (the
  tab's shell), and reads the working directory of that terminal's
  foreground process. Attaching and ticking the check box probe once too.
  Probing stops once the shell sends an OSC 7 report.

Rules both sources share:

- Only a changed directory moves the sidebar (`SftpView::shell_moved`).
  Shells re-report at every prompt, so a user browsing elsewhere stays put
  until the shell `cd`s.
- Unticked, nothing moves and nothing is probed; ticking it again goes to
  the shell's last known directory and probes.
- A directory known before the SFTP task is attached becomes the
  directory the first listing starts at.
- A directory the SFTP user cannot list shows the usual inline error and
  leaves the listing where it was. A foreground process the probe cannot
  read (`su`, `sudo`) leaves the answer at the shell's own directory.

Where probing cannot work, a prompt hook sending OSC 7 still does, e.g.
bash's `PROMPT_COMMAND='printf "\e]7;file://%s%s\a" "$HOSTNAME" "$PWD"'`
(unencoded paths work, except that one containing `%` is misdecoded or,
as an invalid escape, dropped by VTE).

## Drag and drop

Uploads accept both `GdkFileList` (GTK 4 file managers) and
`text/uri-list` (older ones: `file://` URIs with an empty or `localhost`
host, percent-decoded). gtk4-rs does not bind the one-target multi-type
API, so the listing carries two drop targets.

## Availability

SSH sessions only: RDP, VNC, serial and local tabs have no SSH connection.
On their tabs the SFTP panel shows its placeholder and `toggle_sftp` is
disabled.

## Limitations

- Jumped sessions have no SFTP task (doc 04 §Jump hosts): the sidebar shows
  "SFTP is not available for jumped sessions yet", and nothing follows the
  shell.
- No right-click menu, and no dragging files out of the listing.
- Following the shell: plain-tab probes need a Linux remote. Tabs to the
  same user@host share one connection, and probing tells their shells
  apart only while at most one of them is a plain shell; with two, it
  gives no answer. A tmux tab follows the active pane, so switching panes
  shows after the next command. Probes see physical paths where OSC 7
  reports `$PWD`, so a symlinked directory may show under its target's
  name.
- No file browser for local-shell tabs.
