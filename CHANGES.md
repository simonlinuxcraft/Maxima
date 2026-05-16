# Local Modifications to Maxima (CLI Tree)

This file tracks modifications made to this local copy of Maxima
(used by the Kyber CLI build at `Kyber/CLI/`). The upstream is
[ArmchairDevelopers/Maxima](https://github.com/ArmchairDevelopers/Maxima).

These modifications remain licensed under **GPL-3.0**, matching the
upstream license. Each entry below documents what was changed, when,
and why, in line with GPLv3 §5 (modification notice).

A second copy of this Maxima tree lives at `Kyber/ThirdParty/Maxima/`
(used by the Flutter Launcher build). The LSX patches below are kept
byte-identical between the two trees. Two divergences exist as of
2026-05-05 and are documented in the Launcher-tree CHANGES.md:

1. `unix/wine.rs` — the Launcher tree carries a `setup_wine_registry`
   refactor (per-key `reg add` calls instead of `.reg` import) and an
   added `UMU_ZENITY=1` env on the wine command spawn. The CLI tree
   still uses the original `.reg`-file flow. The CLI build is the
   one currently in production use, so the refactor is not yet
   replicated here pending verification it does not regress the CLI
   launch path.
2. `Cargo.toml` — the Launcher tree pins `flate2 = "=1.0.28"`; the
   CLI tree leaves it at `"1.0.28"` (semver-compatible). Reason
   documented in the Launcher CHANGES.md.

Both divergences are intentional but should be reconciled when the
trees are merged.

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
Identical to the patch in the parallel `Kyber/ThirdParty/Maxima/`
tree. The `GetConfig` response advertises `EbisuSDK/ACHIEVEMENT`
services. When Battlefront 2 calls `QueryAchievements` and the
request type is not handled, the LSX connection state diverges from
what the game expects. Symptom on Linux: on quit the game returns to
the Frostbite main menu instead of the desktop. An empty but
well-formed response keeps the connection consistent and resolves
the bug.

**Upstream tracking**
Will be submitted as a single PR to `ArmchairDevelopers/Maxima`,
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
A quit-time hang on Linux (game stays in Frostbite main menu instead
of returning to desktop) coincides with a generic `LSX connection
closed` log line, with no indication whether the close is a clean
TCP FIN from BF2 or an internal IO error in Maxima. The added
diagnostics distinguish the two without exposing token/auth contents
that other `LSXConnectionError` variants may carry.

**Upstream tracking**
Diagnostic-only patch; will be offered upstream alongside the
`QueryAchievements` PR if useful, otherwise kept as a local
modification.
