# SFTP Sidebar

A remote file browser for each SSH tab, shown in the side dock's SFTP
panel (doc 01): navigate, upload, download, delete, rename, mkdir, and
drag files in. GTK half in
`src/ui/sftp_sidebar.rs` (its display-free model in
`src/ui/sftp_sidebar/model.rs`), tokio half in `src/sftp/`.

## Layout

Each SSH tab owns its sidebar, but the tab's page holds only the
terminal: the side dock's SFTP panel (`src/ui/side_dock.rs`) hosts the
current tab's sidebar and swaps it as the notebook switches tabs. Tabs
without one (local, serial, RDP, VNC, or no tab at all) get a
placeholder. A redial replaces the page's sidebar, ending the old SFTP
task, and the panel hosts the new one.

The sidebar is a path bar (Enter navigates), a toolbar
(Upload, Download, Delete, Rename, Refresh, New folder), an inline error
line, the listing (name, size, modified, permissions; directories first,
case-insensitive; a `..` row except at `/`), a progress bar with Cancel,
and a status line. `toggle_sftp` (Ctrl+Shift+B) switches the dock to the
SFTP panel, or back to the panel it came from.

## Connection Model

The sidebar runs over the tab's **shared** `SshConnection` (doc 04) — no
second process, no second login. `SftpSidebar::attach(connection)` spawns
one SFTP task per sidebar; until the connect flow attaches it, the sidebar
is empty with its toolbar disabled. The task opens a session channel with
the `sftp` subsystem and turns it into a `russh_sftp` session on the first
command. Dropping the last sidebar handle ends the task; the connection
stays with the supervisor.

With the `sftp_on_connect` setting on (the default, doc 06), a sidebar
attaches when its tab connects, and a connect the user started on the
current tab brings the SFTP panel forward; a reconnect never switches the
dock. Off, a new SSH tab's sidebar starts hidden and the connect flow
only hands it the connection: the task starts the first time the SFTP
panel has that tab's sidebar in view, so a user who never looks never
opens an SFTP channel. A sidebar already shown when the connect lands
attaches at once.

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

- GTK code never awaits: it sends with `try_send` (parking overflow for an
  idle flush, doc 02 §Backpressure) and awaits the oneshot in a
  `spawn_local` future.
- Every command but `Cancel` is answered exactly once — with `Err` when the
  session is dead. Any failure drops the cached session and the next
  command reopens it, so a server closing the subsystem shows up as an
  error and recovers.
- `Cd` doubles as the listing command. There is no `getcwd`, so the path
  bar shows the requested path; browsing starts at `.`, the SFTP home.
- `Delete` stats first: russh-sftp 3 splits removal into file and
  directory calls (directories must be empty).

## Transfers

Downloads and uploads run as their own tasks, registered by a
`transfer_id` the client pre-generates, so `Cancel` and browsing stay
responsive mid-transfer. 32 KiB chunks; progress samples go out with
`try_send` (a stale-but-moving bar is fine, a stalled transfer is not); the
bar shows whichever transfer updated last and Cancel targets the newest.
An upload's final flush and close must succeed or the transfer fails,
since the remote file may be incomplete. Every successful operation
refreshes the listing.

## Drag and drop

Uploads accept both `GdkFileList` (GTK 4 file managers) and
`text/uri-list` (older ones: `file://` URIs with an empty or `localhost`
host, percent-decoded). gtk4-rs does not bind the one-target multi-type
API, so the listing carries two drop targets. Dragging files *out* is not
implemented; Download uses a file chooser.

## Availability

SSH sessions only: RDP, VNC, and serial have no SSH connection. On their
tabs the SFTP panel shows its placeholder and `toggle_sftp` is disabled. A local-shell file browser over `std::fs`
is a possible later addition.

## Later

Follow the shell's working directory: VTE reports OSC 7 as
`current-directory-uri`; the sidebar could `Cd` there.
