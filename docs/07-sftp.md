# SFTP Sidebar

A remote file browser for each SSH tab, shown in the side dock's SFTP
panel (doc 01 §Side dock): navigate, upload, download, delete, rename,
create folders, drag files in, open files in a local application, and
follow the shell's working directory. A local-shell tab gets the same
sidebar over this machine's own files (§Local tabs).

Code: `src/sftp/` (the tokio half: task, commands, shell-directory probe,
and the local file task),
`src/ui/panels/sftp_sidebar.rs` (the GTK half), `src/ui/panels/sftp_sidebar/model.rs`
(its display-free model: rows, columns, toolbar rules, opened files).

## Layout

Each SSH tab owns its sidebar, and each local-shell tab its own (§Local
tabs), but the tab's page holds only the terminal: the side dock's SFTP
panel hosts the current tab's sidebar and swaps it as the notebook
switches tabs. Tabs without one (serial, RDP, VNC, Home) get a
placeholder. Whenever the tab lets go of its
connection (a redial, Disconnect, a lost connection, the shell's exit or
a transport that fails to connect, doc 03 §Reconnect), the old sidebar
detaches, ending its SFTP task and cancelling any running transfer
(§Transfers), and an empty one replaces it in the panel, keeping only the
files the old one had open in editors (§Opening files). A closing tab's
sidebar detaches the same way (doc 03 §Closing a tab).

The sidebar, top to bottom:

- a path bar (Enter navigates);
- a toolbar: Upload, Download, Delete, Rename, Refresh, New folder;
- an inline error line;
- the listing (§Listing);
- a progress bar with Cancel and a status line;
- the "Follow terminal directory" check box (§Following the shell).

The toolbar's and the rows' icons ship in the binary (doc 09 §Icons), so
they are the same on every desktop.

`toggle_sftp` (Ctrl+Shift+B) switches the dock to the SFTP panel, or back
to the panel it came from; a panel hidden by `toggle_session_tree` comes
back on SFTP. While focus is in the panel (not its path bar), the
`sftp_*` shortcuts press the toolbar's Upload, Download, Rename and Delete
buttons, enabled or not as clicks are (doc 11 §Handlers). Download and
Upload use file choosers; Rename and New folder ask for a name; Delete
asks for confirmation.

## Listing

Directories first, then files, case-insensitive; a `..` row except at `/`.
Four columns:

| Column | Shows |
|---|---|
| Name | the row's icon (folder, file, or parent folder for `..`) beside its name |
| Size | a file's size in binary units, against the column's right edge; empty for a directory |
| Modified | `YYYY-MM-DD HH:MM`, UTC |
| Permissions | `rwxr-xr-x` |

Name takes the width the others leave. A name too long for it is cut in
the middle, keeping its extension, and shown whole in a tooltip. Name
never gets narrower than about 14 characters: in a panel too narrow for
every column the listing scrolls sideways instead. Columns are resized by
dragging a header's edge.

A right-click on any column header lists Size, Modified and Permissions,
each with a check box: unticking one hides that column. Name always
shows. The choice is one for every tab, stored in the `sftp_columns`
settings row (doc 13 §Settings rows) and applied to another tab's sidebar
when the panel next shows it.

Double-clicking a row, or Enter on it, goes into a directory, up from
`..`, or opens a file (§Opening files). A right-click on a row selects it
and offers Open, the same, and for a file Open With….

## Opening files

Opening a file downloads it to a local copy and hands that to the desktop
(`GtkFileLauncher`: the OpenURI portal where there is one, else GIO), which
starts the default application for the file's type; Open With… asks which
application instead. The transfer shows in the progress bar as
`Open <name>` and can be cancelled like any other. The download lands in
a staging file and is moved over the copy only once it is whole, so a
failed or cancelled one never leaves half a file where an editor reads.

While the tab stays open the sidebar watches the copy, and each version
the application saves is uploaded over the remote file (`Save <name>` in
the status line), so saving in the editor saves to the server:

- The copy's directory is watched, not the file, so both ways editors
  save are seen: writing the file in place, and renaming a new file over
  it. A look at the copy waits until 300 ms pass without a change there,
  and uploads only when the copy's size or modification time differs from
  the version the server has (`EditSync`).
- One transfer per file at a time: a save during an upload is sent when
  that upload ends. A failed upload shows in the status line, and the
  next save, or opening the file again, sends it again.
- Opening a file that is already open downloads it again into the same
  copy, so the application shows what the server has now. A save is
  never downloaded over: if the copy holds a version the server lacks,
  that version is uploaded and shown instead, and a save that lands
  while the download runs is kept and uploaded, the download dropped.
- When the tab lets go of its connection (§Layout) the sidebar that
  replaces the old one takes the opened files over and keeps watching
  them. A save made with no session shows "SFTP needs a live SSH
  connection" and is uploaded when the tab connects again, as is one
  whose upload the lost connection cut off.

Copies live in `$XDG_CACHE_HOME/wayterm/sftp-edit/<pid>-<n>/<name>` (the
staging file is `<pid>-<n>.part` beside the directory), one directory per
opened file. `sftp-edit` must be the user's own directory with no access
for anyone else, or nothing is opened; without `XDG_CACHE_HOME` or `HOME`
there is no such place, and a shared temporary directory is never used.
The copies are removed when the tab closes, or its session turns out to
be a jumped one: a later save has nowhere to go, and fails in the editor
rather than landing in a file nobody uploads. Whatever is left is removed
when Wayterm starts and when it quits.

Opening needs no new `SftpCommand`: it is a `Download`, then an `Upload`
per save.

## Connection model

The sidebar runs over the tab's **shared** `SshConnection` (doc 04
§Connection supervisor): no second process, no second login. Attaching
spawns one SFTP task per sidebar; until then the sidebar is empty with its
toolbar disabled. The task opens a session channel with the `sftp`
subsystem and turns it into a `russh_sftp` session on the first command.
Dropping the last sidebar handle ends the task, once any transfer still
running has finished (its futures hold the sidebar). Detaching (§Layout)
shuts it down at once: the command in progress is dropped, queued ones
are discarded, and running transfers are cancelled. Either way the
connection stays with the supervisor.

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
  the session is dead — unless the task shuts down first: then its reply
  channel closes unanswered, which the sidebar shows as the session
  ending. Any failure drops the cached SFTP session and the
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
refreshes the listing. Cancel takes effect at whatever step a transfer is
on, even one waiting on a stalled server. Ending the tab's session, or
closing the tab, cancels every running transfer, as Cancel does (§Layout).

## Following the shell

While "Follow terminal directory" is ticked (on by default, per tab, kept
across a redial), the sidebar shows the directory the tab's shell is in.
It learns that two ways:

- **OSC 7.** A shell that reports its directory (`file://host/path`,
  percent-encoded) makes VTE emit `current-directory-uri-changed`, or the
  Ghostty view call its directory hook (doc 02 §GhosttyBackend); the tab
  hands the URI to a `DirectoryFollower`. The tab connects to the
  terminal widget directly, to VTE's signals or the Ghostty view's
  matching hooks, so `TerminalBackend` and `TerminalEvent` are unchanged.
  The first report pins the host:
  reports naming another host come from a nested `ssh` inside the
  session, whose paths are not on this connection. This is a heuristic:
  if the session's own shell never reports, the first nested host that
  does is pinned. A local tab knows its host instead (§Local tabs).
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

## Local tabs

A local-shell tab (doc 06 §Local shell) has no connection, and its
sidebar browses the machine Wayterm runs on. It is the same sidebar
sending the same `SftpCommand`s: `SftpClient::local()` spawns a task
(`src/sftp/local.rs`) that answers them from the file system, with no
session to open or lose. What differs:

- **When it reads.** The task starts the first time the SFTP panel has
  the tab's sidebar in view, whatever `sftp_on_connect` says, and opening
  a local tab never brings the panel forward: a tab nobody browses reads
  no directory. The sidebar browses while the tab's shell runs. When the
  shell exits or the tab disconnects an empty sidebar replaces it, as on
  an SSH tab (§Layout); until the tab connects again its path bar
  answers "Browsing files needs the tab's shell running".
- **Paths** are absolute. The first listing is the directory the shell
  starts in: the session's `local_working_dir`, else Wayterm's own
  (doc 06 §Local shell), unless the shell reported another by then. In
  the path bar `~` and `~/…` mean the home directory, a relative path
  starts at the listed directory, and `.` and `..` steps are taken by
  name, as a shell's `cd` takes them (`local_target`).
- **Listing.** A symbolic link shows as what it points to, so a linked
  directory opens; a dangling one shows as itself. The whole directory is
  read in one pass on the blocking pool.
- **Upload and Download both copy a file.** Upload copies the chosen or
  dropped files into the listed directory; Download copies the selected
  file to where the file chooser says. A copy keeps the file's mode and
  replaces a file already there, as a transfer does. A file is never
  copied onto itself, under its own name or through a link: creating
  the target would empty it.
- **Rename** refuses a name that is taken, as an SFTP server does;
  `rename(2)` alone would replace the other file.
- **Delete** removes a link, never what it points to; directories must be
  empty. Nothing goes to the trash.
- **Opening a file** hands the file itself to the desktop: no copy, no
  watching, nothing to upload (§Opening files does not apply).
- **Following the shell** uses OSC 7 only; nothing is probed. The
  follower takes reports from this machine alone: a URI with no host,
  with `localhost`, or with this machine's host name, with or without
  its domain. A nested `ssh` reports another host and is ignored, even
  when the local shell itself never reports.

## Availability

SSH and local-shell sessions: RDP, VNC and serial tabs have no file
system to browse. On their tabs the SFTP panel shows its placeholder and
`toggle_sftp` is disabled.

## Limitations

- Jumped sessions have no SFTP task (doc 04 §Jump hosts): the sidebar shows
  "SFTP is not available for jumped sessions yet", and nothing follows the
  shell.
- The row menu has only Open and Open With…; the toolbar's operations
  are not in it. No dragging files out of the listing.
- A cancelled transfer leaves its partial file behind, local or remote.
- Opened files: a save overwrites the remote file as it is then, without
  checking whether it changed on the server since it was opened, and
  writes it in place, so a failed upload can leave it incomplete until
  the next one (SFTP has no replace-by-rename every server accepts, and
  a renamed-in file would lose the original's owner and mode). The
  editor is not told when an upload fails; the status line is. Closing
  the tab, or quitting, removes the copies, a save not yet uploaded
  included (one made in the last 300 ms, during a transfer, or while
  disconnected), and an editor still holding the file has to save it
  elsewhere. Which application opens a file is the desktop's
  choice (Open With… to pick another); Wayterm has no editor setting of
  its own. Open With… asks only through the portal: without one GTK
  starts the default application, as Open does. The Flatpak's hand-off
  through the portal is not verified (doc 14 §Limitations).
- The columns cannot be reordered or sorted by, and their widths are not
  remembered.
- Following the shell: plain-tab probes need a Linux remote. Tabs to the
  same user@host share one connection, and probing tells their shells
  apart only while at most one of them is a plain shell; with two, it
  gives no answer. A tmux tab follows the active pane, so switching panes
  shows after the next command. Probes see physical paths where OSC 7
  reports `$PWD`, so a symlinked directory may show under its target's
  name.
- Local tabs (§Local tabs): the sidebar follows only a shell that sends
  OSC 7. One that sends none (bash without a prompt hook, any shell
  inside tmux) leaves the listing where it is; there is no local probe.
  The toolbar still says Upload and Download where both copy. Rename's
  check for a taken name and the rename are two steps, so a file created
  between them is replaced. A name that is not valid UTF-8 lists with
  replacement characters and cannot be opened or changed. In the Flatpak
  the sidebar sees what the sandbox does: the home directory, and the
  runtime's files elsewhere (doc 14 §Flatpak).
- Dragged narrower than the sidebar needs, the SFTP panel is cut off on
  the left (doc 01 §Side dock).
