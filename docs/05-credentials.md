# Credentials

Secrets never touch SQLite, logs, argv or the environment of a child that
does not need them. A session row only records which auth method to use;
at connect time a credential chain finds the secret. Import and export
never include secrets.

Code: `src/transport/credentials.rs` (sources, the chain),
`src/transport/credentials/chain.rs` (chain order, prompt wording),
`src/ui/tab/credentials.rs` (the chain per tab), `src/ui/settings/credentials.rs`
(the Settings page), `src/ui/session_dialog.rs` (save on OK),
`src/ui/session_tree.rs` (delete with the session).

## Credential chain

`CredentialManager` walks its sources in priority order, first hit wins:

1. **Keyring** — the OS keyring (§Keyring).
2. **Bitwarden** — only when enabled (§Bitwarden).
3. **Prompt** — the challenge dialog with one masked field ("Password: ",
   or "Passphrase: " for a key passphrase); always last. Cancelling it
   aborts the connect cleanly.

The chain is built per connect from the Settings rows, so a toggle applies
to the next connect. When the keyring cannot be reached, the prompt's
instructions line reads "Keyring unavailable — prompting for
credentials.", so a prompt that used to be answered silently is explained.

Every protocol uses the same chain, with a secret type per need:

| Secret type | Asked for by |
|---|---|
| `password` | SSH password method (doc 04 §Authentication) |
| `key_passphrase` | an encrypted SSH key |
| `vnc_password` | VNC (doc 08 §VNC) |
| `rdp_password` | RDP (doc 08 §RDP) |

Storing walks the same list and uses the first source that can; a source
that cannot store answers `Unavailable` and the walk moves on. Only the
keyring stores.

## Credential sources

### Keyring

Any Secret Service (`org.freedesktop.secrets`) provider through `oo7`:
GNOME Keyring, KWallet's Secret Service bridge, or KeePassXC with its
Secret Service integration enabled — which makes a KeePassXC database a
drop-in store with no KeePass-specific code. Sandboxed (Flatpak), `oo7`
uses the portal-backed file keyring instead.

Items use the libsecret schema `com.alex-eckstein.wayterm.Session` with
the attributes `application` (`com.alex-eckstein.wayterm`), `session_id`,
`session_name` and `secret_type`, labelled "Wayterm — `name` (`type`)".
The application id is baked into the schema and the XDG paths: changing it
orphans stored credentials.

Lookups match attributes exactly (never a substring): by session id
first, then by session name, so items made by other tools for that
session are found. An unreachable keyring is a miss, and the next source
answers.

### Bitwarden

The `bw` CLI, read-only and off by default:

- A session looks up the item named `wayterm/<session name>` (exact name,
  no aliases) with `bw get password --raw wayterm/<name> --nointeraction`;
  trimmed stdout is the secret, and it answers whichever secret type the
  session's method asks for (a session has one method, so one secret).
- The vault session key is read from `BW_SESSION` in Wayterm's
  environment when the source is built and handed to the child through its
  environment only — never argv, which any local user can read in
  `/proc/<pid>/cmdline`. Wayterm never writes `BW_SESSION` itself.
- Each lookup runs one `bw` with a 10 s timeout (then it is killed);
  nothing is cached between connects. A missing binary, non-zero exit,
  timeout or empty output is a logged miss that falls through to the
  prompt.
- Storing answers `Unavailable`.

## Settings

The Settings window's Credentials page (doc 13) writes two unseeded rows:

- `bitwarden.enabled` — **off** unless explicitly `1`/`true`/`yes`/`on`.
  Unlike the other boolean rows, running an external CLI on every connect
  is opt-in.
- `bitwarden.bin` — the CLI path; blank or missing means `bw`.

The page also shows whether the keyring and the CLI answer. The CLI probe
runs `bw status --nointeraction` without `BW_SESSION` and keeps only the
`status` field (`unauthenticated`, `locked`, `unlocked`).

## Saving and deleting

- **Save on OK.** For an SSH session on the password method, the session
  dialog takes a password and offers **Save password to keyring**
  (disabled until a password is typed). On OK the session is saved first,
  then the password goes to the keyring under the saved session's id as
  `password`. A refused store is shown inline ("Could not store password:
  …") and the session stays saved. No other secret type has a save
  control.
- **Delete with the session.** Deleting a session deletes its keyring
  entries for every secret type. A keyring failure is logged and never
  blocks the delete.
- **Clone.** A cloned session gets a new id, so the original's keyring
  entries stay with the original.

## Session identity

Each chain is built for a saved session's row id, and lookups key on it.
A manager with no saved session uses the placeholder id `_live_connect`,
which the keyring never stores or serves, so id-less connects cannot share
one entry. The Bitwarden source keys on the session name instead.

## Limitations

- Only SSH passwords can be saved from the UI. VNC and RDP prompt on every
  connect unless the keyring already holds the secret (stored by another
  tool under the session's id or name) or Bitwarden answers.
- Bitwarden is read-only and matches one exact item name.
- A secret typed at the prompt is not offered for saving.
