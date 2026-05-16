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

## 2026-05-09 — Verify-first locale skip: ~50s warm-launch speedup

**Files touched**
- `maxima-lib/src/unix/wine.rs`
  - `verify_locale_is_english()` now returns `bool` (true when all four
    locale-critical keys read back the expected English values). Old
    behaviour was diagnostic-only (void return). Logging is unchanged.
  - `setup_wine_registry()` runs `verify_locale_is_english()` as a
    pre-flight on entry. When it returns `true`, the function skips
    the entire 15-entry `reg add` loop and returns `Ok(())` early.
    Each reg.exe round-trips through umu-run + pressure-vessel (~3s);
    skipping turns a ~50s warm-launch overhead into ~12s.
- `maxima-lib/src/core/launch.rs`
  - The pre-spawn block now reads `verify_locale_is_english()` first.
    Only when it reports a drift does it call
    `lock_locale_just_in_time()`. If the prefix is intact (the common
    case after a successful previous launch) the four-key JIT re-write
    is skipped entirely.

**Why**
A successful "boot ➜ play BF2 ➜ exit ➜ play BF2 again" cycle was
measured at ~73s per Locale-Setup pass on the user's machine: 48s for
the `setup_wine_registry` 15-entry loop, 14s for `lock_locale_just_in_time`,
11s for `verify_locale_is_english`. Each call is dominated by
pressure-vessel container spawn cost (~3s) — the actual `reg.exe`
work is sub-second.

After the first successful launch, the four critical keys are already
correct in the prefix. There is no value in re-writing them on every
subsequent launch. The verify probe (4 `reg query` calls, ~12s total)
is enough to confirm the state and short-circuit both expensive paths.

**Effect**
- Cold launch (fresh prefix or after a Steam/protonfix tamper):
  ~73s → ~69s (verify costs 12s but prevents redundant follow-up writes
  when setup succeeded).
- Warm launch (state intact from a previous launch):
  ~73s → ~24s (verify only, both writes skipped).

The fall-through path is untouched: when verify reports a mismatch, the
full `setup_wine_registry()` and `lock_locale_just_in_time()` paths run
exactly as before, so anti-tamper coverage is preserved. Only the
pessimistic-write cost was removed.

**Upstream tracking**
Local-only optimisation; depends on the layered locale-lock patch
above. Not portable upstream until that one lands.

---

## 2026-05-09 — Layered locale lock to defeat the BF2 entitlement error

**Files touched**
- `maxima-lib/src/unix/wine.rs`
  - `setup_wine_registry()`: added two more `ENTRIES` for
    `HKCU\Environment\LANG` and `HKCU\Environment\LC_ALL`
    (`en_US.UTF-8`). Wine reads `HKCU\Environment` on session start and
    propagates the values as Win32 process env, which crosses the
    pressure-vessel boundary that filters Linux env vars.
  - `setup_wine_registry()`: traced now via `log::info!` at start
    (number of entries) and at end (entries OK), and `log::debug!` per
    successful `reg add`. Without this every successful run was silent
    and the function looked like it never executed.
  - `setup_wine_registry()`: critical failures (BF2 catalog
    `locale`/`displayname`, `Control Panel\International` triple,
    `Valve\Steam\language`) now abort the launch via `Err(NativeError::Io)`
    instead of returning `Ok(())` after logging a warning that nobody
    saw. The launch dialog can finally surface a real error string
    when the locale chain breaks.
  - New `lock_locale_just_in_time()`: subset of the four locale-critical
    keys, fast `reg add /f` overwrite. Called immediately before the BF2
    spawn (see `core/launch.rs`) so license/cloud-sync/protonfix code
    that runs between `setup_wine_registry()` and the spawn cannot leave
    the keys in a German state.
  - New `verify_locale_is_english()`: queries the four critical keys via
    `reg query`, logs each value, warns if the live value does not match
    `en_US`/`en-US`/`ENU`/`english`. Diagnostic only — never blocks the
    launch. Closes the visibility gap that hid the previous regressions.
- `maxima-lib/src/core/launch.rs`
  - In the bootstrap spawn block (around line 337), added explicit
    `.env("LANG", "en_US.UTF-8").env("LC_ALL", "en_US.UTF-8")`. The
    bootstrap binary is invoked through `Command::new` directly (not via
    `run_wine_command`) and was inheriting `LANG=de_DE.UTF-8` from the
    Flutter host, which then propagated through umu-run / pressure-vessel
    and prompted protonfixes to re-initialise parts of the prefix with
    German values, undoing the earlier `setup_wine_registry()` writes.
  - Right before `let child = child.spawn()`, added the
    `lock_locale_just_in_time()` re-write and the
    `verify_locale_is_english()` probe under `#[cfg(unix)]`. Both are
    best-effort and never block the launch; they exist to (a) close the
    timing window between `setup_wine_registry` and the actual BF2
    spawn, and (b) log the live registry state so the next regression
    has a paper trail.

**Why**
The BF2 "title is installed in a language that you are not entitled to
play" error returned in spite of all earlier fixes. Diagnosis showed
two leaks the existing setup did not cover:

1. `setup_wine_registry()` had no success-path logging at all. When all
   13 `reg add` calls succeeded the function was completely silent and
   it was indistinguishable from a never-called function in a normal
   launch log. Trying to debug a regression with no signal was hopeless.
2. The bootstrap spawn (`Command::new(bootstrap_path()?)` in
   `start_game()`) inherits the host process's environment, which on a
   German system carries `LANG=de_DE.UTF-8`. Even though
   `setup_wine_registry()` had just set the locale-critical Wine
   registry keys, the umu-run sub-tree spawned by the bootstrap
   re-evaluated `$LANG` and protonfixes touched `user.reg`, undoing the
   writes shortly before BF2 read them.

The fix is layered: pre-launch (registry setup + critical-failure
gate), in-flight (explicit env on the bootstrap-spawn), and just-in-time
(four-key re-write + verification immediately before BF2 starts). Each
layer is independent, so a single broken layer does not silently
cascade.

**Upstream tracking**
Locale-port-specific. The BF2 entitlement-check behaviour upstream is
masked because Windows hosts do not exhibit the German-locale leak
through pressure-vessel. Will be offered upstream as an opt-in
"Linux locale hardening" patch.

---

## 2026-05-09 — `registry.rs`: Linux `read_game_path` Steam-library detection

**Files touched**
- `maxima-lib/src/util/registry.rs`
  - Replaced the `todo!()` stub for the Linux variant of
    `read_game_path()` with a real implementation that maps known
    BF2 slugs (`bf2`, `swbf2`, `starwarsbattlefront2`,
    `STAR WARS Battlefront II`) to Steam app id `1237950`, walks the
    Steam library candidate list (`STEAM_LIBRARY_ROOT` env override,
    Flatpak Steam, native `~/.steam/steam`, `~/.local/share/Steam`,
    and the user's `/mnt/Games/SteamLibrary` setup), and reads
    `libraryfolders.vdf` plus the per-game `appmanifest_<id>.acf` to
    derive `<library>/steamapps/common/<installdir>/`.
  - The macOS variant retains its `todo!()` stub.
  - Added a small inline VDF/ACF tokenizer (`parse_libraryfolders_vdf`,
    `parse_appmanifest_acf`) so the change does not introduce a new
    Cargo dependency.
  - Added six inline unit tests under
    `linux_steam_detection_tests` covering the slug map, both
    parsers, and edge cases (missing keys, garbage input).

**Why**
The Linux `todo!()` made every FFI call from the launcher's
`get_game_dir(slug)` panic — in practice the Rust panic was caught
upstream and surfaced as an empty `String` to Dart, which then
silently disabled every feature that needed the BF2 install path
(mod manager root detection, screenshot path resolution, `bf2.exe`
discovery for the launch flow). The launcher could not be released
on Linux without a working game-path lookup. Steam detection covers
the most common Linux install path; Heroic/Lutris/EA-App users
remain unsupported until follow-up work plugs additional sources
into this code path.

**Upstream tracking**
This file is the first GPLv3 §5(a) modification notice for
`registry.rs`; the Linux branch was previously stubbed upstream.
The patch is portable and will be offered to
`ArmchairDevelopers/Maxima` once we agree on a slug-to-appid
catalog format with upstream (currently hardcoded BF2-only).

---

## 2026-05-07 — `wine.rs`: silence subprocess stdio (no wineconsole popup)

**Files touched**
- `maxima-lib/src/unix/wine.rs` — in `run_wine_command()`:
  - Added `binding.stdin(Stdio::null())` immediately after the `Command::new`
    construction.
  - When `want_output == true`, added `.stderr(Stdio::null())` to the spawn chain.
  - When `want_output == false`, replaced the bare `child.spawn()?` with
    `.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?`.

**Why**
Without these stdio overrides Wine spawns a wineconsole window when
launching console-subsystem helpers (`wine-helper.exe`), because it
treats an inherited terminal-stdin as a sign that an interactive user
is present. Detaching stdin and silencing helper output streams keeps
the helpers headless and matches the Windows GUI behaviour.

**Upstream tracking**
Portable to upstream as a standalone change.

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
