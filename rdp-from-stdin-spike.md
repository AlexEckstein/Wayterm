# RDP `/from-stdin` Spike — Findings and Phase 7 Recommendation

Task P2-SPIKE (doc 10 §Phase 2 "Spike (half-day, parallel)"). Research +
live verification of `xfreerdp /from-stdin[:force]` as the credential path
for doc 08's subprocess-based RDP embedding. Feeds doc 11 §RDP Credentials.

**Headline: doc 11's premise is wrong.** `/from-stdin` is not a FreeRDP 3
feature — it landed in **2.0.0-rc1 (November 2017)** and is present in every
2.x release since. There is no "2.x lacks `/from-stdin`" distro to fall back
for. The real portability hazard is different and worse in a different way:
**the binary name** (`xfreerdp` vs `xfreerdp3`), and a
**certificate-verification interaction** that silently breaks connections.

---

## 1. What is installed / verified here

| Item | Value |
|---|---|
| Host | Debian 13 (trixie), amd64 |
| Package | `freerdp3-x11` 3.15.0+dfsg-2.1+deb13u3 |
| Binary | `/usr/bin/xfreerdp3` (there is **no** `xfreerdp` on trixie) |
| Version string | `This is FreeRDP version 3.15.0 (n/a)` |
| `freerdp2-x11` on trixie | **Not available** — `apt-cache policy freerdp2-x11` returns no candidate |
| `xrdp` (test server) | 0.10.1-3.1+deb13u2, installed for the live test |

`/from-stdin` is present in both the `--help` output and the manpage:

```
$ xfreerdp3 --help | grep -A3 from-stdin
    /from-stdin[: force]              Read credentials from stdin. With <force>
                                      the prompt is done before connection,
                                      otherwise on server request.

$ man xfreerdp3 | grep -A3 from-stdin
       /from-stdin[:force]
           Read credentials from stdin. With <force> the prompt is done before
           connection, otherwise on server request.
```

---

## 2. Distro matrix

Package versions as of 2026-10. `/from-stdin` column is derived from the
upstream `client/common/cmdline.c` at that tag (see §4), not from the manpage
alone.

| Distro | Package | Version | Binary | `/from-stdin` |
|---|---|---|---|---|
| Debian 13 trixie | `freerdp3-x11` | 3.15.0+dfsg-2.1 | `xfreerdp3` | yes (verified locally) |
| Debian 12 bookworm | `freerdp2-x11` | 2.11.7+dfsg1-6~deb12u1 | `xfreerdp` | yes |
| Ubuntu 26.04 resolute | `freerdp3-x11` / `freerdp-x11` | 3.24.2+dfsg | `xfreerdp3` | yes |
| Ubuntu 24.04 noble | `freerdp3-x11` | 3.30.0+dfsg (3.5.1 per USN-8432-1 — see conflict note) | `xfreerdp3` | yes |
| Ubuntu 22.04 jammy | `freerdp2-x11` | 2.6.1+dfsg1 | `xfreerdp` | yes |
| Fedora 45 | `freerdp` | 3.31.1-1.fc45 | `xfreerdp` | yes |
| Fedora 44 | `freerdp` | 3.24.2-1.fc44 | `xfreerdp` | yes |
| Fedora 43 | `freerdp` | 3.16.0-4.fc43 | `xfreerdp` | yes |
| Fedora 42 | `freerdp` | 3.12.0-1.fc42 | `xfreerdp` | yes |
| Arch | `freerdp` | 2:3.31.1-1 | `xfreerdp3` | yes |
| openSUSE Tumbleweed | `freerdp` | 3.31.0-1.1 | `xfreerdp` | yes |
| openSUSE Leap 16.0 | `freerdp` | 3.22.0-1 (3.10.3-160000.2.2) | `xfreerdp` | yes |
| openSUSE Leap 15.6 | `freerdp` | 2.11.2-150600.2.1 | `xfreerdp` | yes |
| RHEL / AlmaLinux 8 | `freerdp` | 2.11.7-3.el8 | `xfreerdp` | yes |
| Kali rolling | `freerdp3` | 3.31.1 | `xfreerdp` (transitional pkg) | yes |

Sources: [Debian trixie filelist](https://packages.debian.org/trixie/amd64/freerdp3-x11/filelist),
[Debian bookworm freerdp2-x11](https://packages.debian.org/bookworm/freerdp2-x11),
[Ubuntu USN-8432-1 (version table for 18.04→26.04)](https://ubuntu.com/security/notices/USN-8432-1),
[Ubuntu noble freerdp3-x11](https://packages.ubuntu.com/noble/freerdp3-x11),
[Fedora freerdp](https://packages.fedoraproject.org/pkgs/freerdp/freerdp),
[Arch freerdp](https://archlinux.org/packages/extra/x86_64/freerdp) and
[its file list](https://archlinux.org/packages/extra/x86_64/freerdp/files/json/),
[openSUSE Tumbleweed manpage](https://manpages.opensuse.org/Tumbleweed/freerdp/xfreerdp.1.en.html),
[openSUSE Leap 15.6 manpage](https://manpages.opensuse.org/Leap-15.6/freerdp/xfreerdp.1.en.html),
[AlmaLinux ALSA-2026:3334](https://errata.almalinux.org/8/ALSA-2026-3334.html),
[Kali freerdp3](https://www.kali.org/tools/freerdp3).

**Conflicts noted, upstream preferred:**
- Ubuntu 24.04: USN-8432-1 lists `freerdp3-x11 3.5.1+dfsg1-0ubuntu1.6`;
  packages.ubuntu.com lists `3.30.0+dfsg-0ubuntu0.24.04.2` for
  noble/noble-updates. The tracker is newer than the June 2026 notice, so
  3.30.0 is current. Either way `/from-stdin` is present.
- Debian bookworm's *rendered* manpage is the 2.10.0 snapshot; the shipped
  package is now 2.11.7. Both contain `/from-stdin`.

**No distro in the matrix lacks the flag.** That kills the doc 10 risk row
("FreeRDP 2.x on some distros lacks `/from-stdin`") as written.

---

## 3. The stdin line format (the actual spec)

**Bare lines, one value per line, no `label:` prefix, no escaping, no
quoting.** The prompt strings that appear on stdout are cosmetic only —
FreeRDP writes them, never parses them.

Read order is **not fixed** — it is whatever FreeRDP still needs at the
moment it prompts. Two variables decide it: whether `:force` was given, and
which credentials are already set on argv.

Source: [`client/common/client.c`](https://github.com/FreeRDP/FreeRDP/blob/3.15.0/client/common/client.c)
`client_cli_authenticate_raw()` prompts in the fixed sequence
`Username` → `Domain` → `Password`, and **skips any field that is already
non-empty**.

| Invocation | stdin lines consumed, in order | When read |
|---|---|---|
| `/from-stdin` (no `/u`) | `username`, `domain`, `password` | on server auth request |
| `/from-stdin` + `/u:user` | `domain`, `password` | on server auth request |
| `/from-stdin:force` (no `/u`) | **`password`**, `username`, `domain` | password **before** connect; rest on auth request |
| `/from-stdin:force` + `/u:user` | **`password`** only | before connect |

The `:force` reordering is the single most surprising finding. `:force` sets
`promptForPassword`, and [`cmdline.c`](https://github.com/FreeRDP/FreeRDP/blob/3.15.0/client/common/cmdline.c)
then reads the password during argv parsing — *before* the TCP connection is
even attempted — while username and domain are still read later from the
`AuthenticateEx` callback. So with `:force` the password is line 1, not
line 3.

Verified by sentinel feeding (each line a distinct token, then checking
which username xrdp's `sesman` received):

```
$ printf 'SENTINEL1\nSENTINEL2\nSENTINEL3\n' | xfreerdp3 /v:... /from-stdin:force
# xrdp-sesman.log: Received system login request from xrdp for user: SENTINEL2
#   → SENTINEL2 was consumed as the USERNAME, i.e. line 1 = password
```

### Interaction with `/u:` on argv

Doc 11 says "force re-prompts even when other credentials were given". More
precisely: **`/from-stdin` never re-prompts for a field that is already set.**
`/u:user` on argv fills `Username`, so the username line is never read and
the stream is not consumed. With `:force` *and* `/u:`, stdin carries exactly
one line — the password. This was confirmed both by the sentinel test
(`/u:rdpuser` + `pw` + two junk lines → `sesman` logged in as `rdpuser`) and
by direct source reading.

Practical consequence for Phase 7: if Wayterm puts the username on argv
(which it should — a username is not a secret, and doc 06 stores it
in the clear), the stdin protocol collapses to a single password line,
which removes the ordering hazard entirely.

### Other stdin details that matter

- **Line terminators**: `\r` and `\n` both terminate a line
  (`freerdp_interruptible_get_line`). No quoting or escape processing; a
  password containing a newline cannot be expressed.
- **Reads are non-blocking and interruptible**: each byte is read via
  `poll()` with `O_NONBLOCK`. Feeding a file that is then closed too early
  produces `freerdp_interruptible_get_line returned Resource temporarily
  unavailable [11]` and `ERRCONNECT_CONNECT_CANCELLED`. When piping from
  Rust, **write all lines and keep the write end open** until the child has
  connected. Confirmed: feeding the right bytes but closing the pipe
  immediately works; feeding the wrong order then closing fails.
- **Echo suppression is bypassed**: with `fromStdin`, FreeRDP reads from
  `STDIN_FILENO` and clears `ECHO` via `tcgetattr` — which is a no-op on a
  pipe. Harmless for a subprocess, worth knowing.
- **`FREERDP_ASKPASS`** is an alternative that FreeRDP honours over
  `/from-stdin` (`freerdp_passphrase_read` checks it first). It exists only
  in **3.10+**, not in 2.x, and it is an *environment variable* — doc 11
  forbids that path. Ignore it; do not set it in the child environment.

### Certificate interaction — the real trap

`/from-stdin` sets `FreeRDP_CredentialsFromStdin`, and
`client_cli_accept_certificate()` then does:

```c
const BOOL fromStdin = freerdp_settings_get_bool(settings, FreeRDP_CredentialsFromStdin);
if (fromStdin)
    return 0;
```

`0` means **reject**. `tls.c` maps the callback result to
`verification_status = -1` → `freerdp_tls_handshake: certificate not
trusted, aborting.` So:

> **`/from-stdin` makes xfreerdp reject every untrusted server certificate,
> silently, without a prompt.**

Reproduced against a stock self-signed xrdp cert:

```
$ xfreerdp3 /v:127.0.0.1:3389 /from-stdin:force /log-level:INFO   # no /cert:ignore
... tls_print_certificate_name_mismatch_error: WARNING: CERTIFICATE NAME MISMATCH!
[ERROR][com.freerdp.crypto] - [freerdp_tls_handshake]: certificate not trusted, aborting.
```

`/cert:tofu` does not rescue it either — with a fresh `known_hosts` the
same rejection path runs. Only `/cert:ignore` (or a pre-populated
`known_hosts`) connects.

This matters a lot for doc 08: Phase 7 must either manage FreeRDP's
`known_hosts` file itself (which means owning TOFU policy in Wayterm, and
conflating it with SSH host-key policy in doc 11) or pass `/cert:ignore`
and accept the MITM exposure. **Neither is decided by this spike** — flag
it as a Phase 7 design decision. Doc 08 currently mentions neither.

### Known FreeRDP bugs in this area

- [issue #10217](https://github.com/FreeRDP/FreeRDP/issues/10217) —
  `--from-stdin` did not work in xfreerdp3 3.5.1 (Arch).
- [issue #11922](https://github.com/FreeRDP/FreeRDP/issues/11922) —
  `/from-stdin:force` not working in 3.17.2 (sdl-freerdp).
- 3.20.1 ChangeLog: *"Fix a regression reading passwords from stdin"* plus
  *"[utils] fix from-stdin (#11965)"* and *"From stdin and sdl-creds improve
  (#12050)"*.

So the flag is present but was not uniformly functional until ~3.20. **Pin a
minimum of FreeRDP 3.20** (or accept testing on 3.15), and re-run doc 10's
CI matrix when that harness exists. Debian trixie's 3.15.0 and Ubuntu
noble's 3.5.1 both predate these fixes.

---

## 4. Provenance

`/from-stdin` entered in **2.0.0-rc1**, from the upstream ChangeLog:

> `client add option to force password prompt before connection (/from-stdin[:force])`

— [FreeRDP 2.0.0-rc1 ChangeLog](https://raw.githubusercontent.com/FreeRDP/FreeRDP/2.0.0-rc1/ChangeLog)

Confirmed present in `cmdline.c` at tags 2.0.0, 2.0.0-rc1, 2.1.0, 2.4.0,
2.6.1, 2.10.0, 2.11.2, 2.11.7, and 3.15.0. The 2.x handler is functionally
identical to 3.x's, modulo the 3.x `freerdp_auth_reason` refactor:

- [2.11.7 `cmdline.c`](https://github.com/FreeRDP/FreeRDP/blob/2.11.7/client/common/cmdline.c#L2851)
- [3.15.0 `cmdline.c`](https://github.com/FreeRDP/FreeRDP/blob/3.15.0/client/common/cmdline.c#L5171)
- [2.11.7 `client.c` `client_cli_authenticate_raw`](https://github.com/FreeRDP/FreeRDP/blob/2.11.7/client/common/client.c#L356)
- [3.15.0 `client.c`](https://github.com/FreeRDP/FreeRDP/blob/3.15.0/client/common/client.c)

Only string-padding differs (`"Username: "` in 2.x vs `"Username:        "`
in 3.x). Nothing in the stdin contract changed across the 2→3 boundary.

### What other clients do

**Remmina does not have a 2.x fallback problem, because it never passes
credentials to a subprocess.** It links libfreerdp as a library and installs
its own `AuthenticateEx` callback that pops a GTK dialog — no argv, no
stdin, no temp file. Its `plugins/rdp/rdp_plugin.c` carries
`#if FREERDP_VERSION_MAJOR >= 3` / `< 3` guards at a dozen sites to build
against both major versions. That is the doc 08 long-term answer, and it is
the only approach with no credential-plumbing question at all. It is not
available to Wayterm's subprocess design without taking a C dependency on
libfreerdp.

Sources: [Remmina `rdp_plugin.c`](https://github.com/FreeRDP/Remmina/blob/master/plugins/rdp/rdp_plugin.c),
[Remmina MR !2554 "update to build for stable-3.0"](https://gitlab.com/Remmina/Remmina/-/merge_requests/2554),
[FreeRDP3 migration notes](https://github.com/FreeRDP/FreeRDP/wiki/FreeRDP3-migration-notes).

---

## 5. Live test — RAN, and it passed

Real end-to-end verification against a local `xrdp` 0.10.1 on
`127.0.0.1:3389`, headless via `Xvfb :99`, using a throwaway local user. No
real credentials were involved; the test account and its password were
discarded afterwards.

**Setup:** `xrdp` + `xrdp-sesman` on port 3389; one throwaway user with a
randomly generated password held in a file, never in argv or env.

### Test 1 — credentials transfer and authenticate

```
$ printf '%s\nrdpuser\n\n' "$PW" | \
    xfreerdp3 /v:127.0.0.1:3389 /from-stdin:force /cert:ignore /log-level:INFO /w:800 /h:600
```

`/var/log/xrdp-sesman.log`:

```
Received system login request from xrdp for user: rdpuser IP: ::ffff:127.0.0.1
Access permitted for user: rdpuser
Received sys login status for rdpuser : logged in
```

A live 800x600 window was mapped on the Xvfb display
(`0x200002 "FreeRDP: 127.0.0.1"`).

### Test 2 — differential (proves the password was actually used)

Same command with a deliberately wrong password:

```
[ERROR] pam_authenticate failed: Authentication failure
[INFO ] AUTHFAIL: user=rdpuser ip=::ffff:127.0.0.1
```

and the correct password restored `logged in`. The server distinguishes
them, so the value really arrived over stdin and was not being ignored.

### Test 3 — credentials never on argv or in env

While a session was connected:

```
$ tr '\0' '\n' < /proc/104234/cmdline
xfreerdp3
/v:127.0.0.1:3389
/from-stdin:force
/cert:ignore
/log-level:INFO
/w:800
/h:600
/size:800x600

$ grep -c "$PW" /proc/104234/cmdline     → 0
$ tr '\0' '\n' < /proc/104234/environ | grep -c "$PW"  → 0
```

The doc 11 requirement holds. Note the username *is* on argv when passed
via `/u:` — acceptable per doc 06, which stores usernames in the clear.

### Test 4 — line-order sentinel

Fed `SENTINEL1\nSENTINEL2\nSENTINEL3\n` with `:force` and no `/u:`.
`sesman` reported `for user: SENTINEL2`, i.e. line 2 was the username and
line 1 the password. Matches the source reading and the table in §3.

### Test 5 — `/u:` + `:force` consumes one line

```
$ printf '%s\nLEFTOVER1\nLEFTOVER2\n' "$PW" | \
    xfreerdp3 /v:... /u:rdpuser /from-stdin:force /cert:ignore
# sesman: Received sys login status for rdpuser : logged in
```

The two junk lines were never read. Confirms §3's last row.

### Test 6 — certificate rejection (§3)

Ran without `/cert:ignore` against the stock self-signed xrdp cert:
`freerdp_tls_handshake: certificate not trusted, aborting.` Also with
`/cert:tofu` and a wiped `known_hosts`.

---

## 6. Recommendation for Phase 7

1. **Drop the doc 10 risk row and the doc 11 fallback plan.** There is no
   2.x-without-`/from-stdin` case. Retiring that fallback also removes a
   whole branch of speculative work: no "external window + xfreerdp GUI
   prompt + settings note" mode needs to exist. Phase 7 supports RDP
   credential-free embedding on *every* distro in §2.
2. **Probe for the binary by trying both names.** `xfreerdp3` on
   Debian 13 / Ubuntu 24.04+ / Arch / Kali-transitional; plain `xfreerdp`
   on Fedora, openSUSE, Ubuntu 22.04, RHEL 8. Do not hardcode either.
   Alternatively detect the installed major version from the package
   (`freerdp2-x11` vs `freerdp3-x11` vs `freerdp`).
3. **Always pass the username on argv and the password on stdin.** With
   `/u:` set, `/from-stdin:force` reads exactly one line. This removes the
   `:force` ordering inversion (§3) from the implementation surface
   entirely — no conditional line-count logic, nothing to get wrong.
   Invoke as `/v:host /u:user /from-stdin:force` with stdin carrying
   `<password>\n`.
4. **Keep the child's stdin pipe open until connected.** FreeRDP reads
   non-blockingly; closing early aborts the handshake
   (`ERRCONNECT_CONNECT_CANCELLED`). Write the password, flush, hold the
   write end, and drop it after the connection reaches an authenticated
   state.
5. **Raise a Phase 7 design question on certificates** — this spike does not
   resolve it. `/from-stdin` silently rejects untrusted certs, so Wayterm
   must choose between owning FreeRDP's `known_hosts` (and thus TOFU policy,
   which doc 11 already defines for SSH) or `/cert:ignore`. Note that
   FreeRDP's `known_hosts` is a *different file* from OpenSSH's, so "reuse
   the doc 11 SSH host-key store" is not free.
6. **Pin FreeRDP ≥ 3.20** if you want the from-stdin fixes from §3, or
   accept that Debian trixie (3.15.0) and Ubuntu noble (3.5.1) are running
   code that had known regressions here. Debian trixie verified clean in
   testing above despite being pre-3.20.
7. **Prefer the libfreerdp C API long-term** — unchanged from doc 08/11, and
   §4 strengthens the case: it is what Remmina does, and it removes the
   argv/stdin/cert plumbing questions rather than answering them.
8. **Re-run this against doc 10's CI matrix** when it exists, at minimum
   across one 2.x distro and one 3.x distro, to confirm the §3 line-order
   table holds on both (source says it does).

### Docs to update (not done here — out of scope for this spike)

- `10-build-order.md` §Phase 2 checklist item + risk table row 3.
- `11-ssh-auth-security.md` §RDP Credentials — replace the "2.x lacks
  `/from-stdin`" fallback paragraph; correct the `:force` re-prompt claim.
- `08-rdp-vnc-embedding.md` §Password Security — same, plus the certificate
  interaction, which is currently unmentioned.

---

## Method note

Versions for non-Debian distros come from vendor trackers and manpage
renderers, not from installation — only Debian 13 trixie was tested on this
host. Code citations are to upstream tags via `raw.githubusercontent.com`
and were read directly. Where a tracker and a security advisory disagreed
(Ubuntu 24.04), the newer tracker was preferred and the disagreement is
recorded in §2. No feature code, no `Cargo.toml` changes.
