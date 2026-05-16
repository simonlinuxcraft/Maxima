# Local Modifications to Maxima

This file tracks modifications made to the upstream
[ArmchairDevelopers/Maxima](https://github.com/ArmchairDevelopers/Maxima)
sources as part of the Kyber Linux port.

These modifications remain licensed under **GPL-3.0**, matching the
upstream license of Maxima. Each entry below documents what was changed,
when, and why, in line with GPLv3 §5 (modification notice).

The intent is to upstream every change here as a pull request to
`ArmchairDevelopers/Maxima`. Until merged, this file serves as the
authoritative log of local divergence.

A second copy of this Maxima tree lives at
`Kyber/CLI/ThirdParty/Maxima/`; the LSX patches below are kept
byte-identical between the two trees, while the `wine.rs` refactor
and the `flate2` pin are currently Launcher-tree only (see entries
below).

---

## 2026-05-05 — `wine.rs`: registry-setup refactor + UMU_ZENITY env

**Files touched**
- `maxima-lib/src/unix/wine.rs`
  - Added `.env("UMU_ZENITY", "1")` to the wine command spawn
    (line 261). This forces umu-launcher to keep its zenity-based
    progress UI active even when `DISPLAY`/`WAYLAND_DISPLAY` are
    inherited from the launcher process, so the user sees download
    progress for first-run prefix bootstrap.
  - Replaced the original `.reg`-file + `regedit`-import flow in
    `setup_wine_registry()` with explicit `reg add` calls routed
    through `run_wine_command()` (lines ~379-475). Each entry now
    runs inside the same umu-run / pressure-vessel context as the
    actual game launch and honours `/reg:32` / `/reg:64` view flags
    so 32-bit Origin clients can read keys under
    `Origin Games\1035052` (BF2 catalog ID).

**Why**
The `.reg` + `regedit` approach was unreliable on Linux:
1. `regedit` started inside the umu pressure-vessel container did not
   always run protonfixes, leaving Origin Games keys partially
   configured.
2. The `.reg` import did not honour the WoW6432Node redirector, so
   32-bit Origin clients could not find their keys.
3. Without a `Locale` value under the Origin Games key,
   `GetUserDefaultLCID()` reported the host's locale (e.g. `de_DE`)
   and BF2's entitlement check aborted launch with "The title is
   installed in a language that you are not entitled to play".

This refactor currently lives in the Launcher-tree copy of Maxima
only; the CLI-tree copy retains the original `setup_wine_registry`.
Synchronisation between the two trees is pending verification that
the refactor does not regress the CLI launch path.

**Upstream tracking**
Will be offered upstream once verified against the CLI path. The
refactor is portable across both trees in principle.

---

## 2026-05-05 — Cargo dependency: `flate2` pin

**Files touched**
- `maxima-lib/Cargo.toml` (line 45)
  - Pinned `flate2 = { version = "=1.0.28", ... }` (exact version)
  - The CLI-tree copy still uses `flate2 = { version = "1.0.28", ... }`
    (semver-compatible range)

**Why**
Cargo's resolver, combined with the workspace-level
`[patch.crates-io] flate2 = { git = "ArmchairDevelopers/flate2-rs" }`
override, occasionally pulled a newer minor version of flate2 into
the Launcher build, breaking the patched-fork compatibility. Pinning
to the exact `=1.0.28` version stabilised the resolved graph.

The CLI-tree copy was not pinned because its build path resolves the
workspace patch differently and did not exhibit the issue. The
divergence is intentional but should be reconciled when the trees
are merged.

**Upstream tracking**
Local-only; the upstream Maxima Cargo.toml does not have this pin,
and the issue may not be reproducible in the upstream CI.

---

## 2026-05-05 — Add `QueryAchievements` LSX request handler stub

**Files touched**
- `maxima-lib/src/lsx/types.rs`
  - New variant `QueryAchievements(LSXQueryAchievements)` in
    `LSXRequestType`
  - New variant `QueryAchievementsResponse(LSXQueryAchievementsResponse)`
    in `LSXResponseType`
  - New `lsx_message!` definitions for `QueryAchievements`,
    `Achievement`, and `QueryAchievementsResponse`
- `maxima-lib/src/lsx/request/mod.rs`
  - Register new `achievements` submodule
- `maxima-lib/src/lsx/request/achievements.rs` (new file)
  - `handle_query_achievements_request` returns an empty
    achievement list response, allowing the game to complete its
    achievement query without breaking the LSX session
- `maxima-lib/src/lsx/connection.rs`
  - Dispatch `QueryAchievements` requests to the new handler

**Why**
The `GetConfig` response already advertises `EbisuSDK/ACHIEVEMENT` and
`EbisuSDK/ACHIEVEMENT_EVENT` services (see
`maxima-lib/src/lsx/request/config.rs`). When Battlefront 2 calls
`QueryAchievements` and the request type is not handled, the LSX
connection state diverges from what the game expects. Symptom on Linux:
on quit the game returns to the Frostbite main menu instead of the
desktop. An empty but well-formed response keeps the connection
consistent and resolves the bug.

**Upstream tracking**
Will be submitted as a PR to `ArmchairDevelopers/Maxima`,
referencing
[Issue #2](https://github.com/ArmchairDevelopers/Maxima/issues/2)
(LSX `QueryAchievements` not implemented).

---

## 2026-05-05 — LSX disconnect diagnostics

**Files touched**
- `maxima-lib/src/lsx/connection.rs`
  - In `listen()`, on `read() == 0`: log `warn!("LSX peer closed socket
    cleanly (FIN received)")` before returning `Closed`.
  - On non-`WouldBlock` read errors: log `warn!("LSX read error:
    kind={:?}")` before returning `Internal(kind)`.
- `maxima-lib/src/lsx/service.rs`
  - In the `listen()` error arm, distinguish `Closed`/`Internal`/other
    by name (token-safe, no `Debug`-print of the full error variant)
    and log `warn!("LSX connection closed: {}", reason)`.

**Why**
Same diagnostic patch as in the parallel `Kyber/CLI/ThirdParty/Maxima/`
tree. A quit-time hang on Linux (game stays in Frostbite main menu
instead of returning to desktop) coincides with a generic
`LSX connection closed` log line, with no indication whether the
close is a clean TCP FIN from BF2 or an internal IO error in Maxima.
The added diagnostics distinguish the two without exposing
token/auth contents that other `LSXConnectionError` variants may
carry.

**Upstream tracking**
Diagnostic-only patch; will be offered upstream alongside the
`QueryAchievements` PR if useful, otherwise kept as a local
modification.
