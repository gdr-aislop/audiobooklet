# Manual UI Test Plan

Touch-driven test checklist for the app as a user sees it — no unit tests, no tooling beyond the
app itself. Derived from `docs/design/ui-spec.md` and covering **the whole spec**, not just the
screens built so far.

Each section header carries a status tag:

- **✅ implemented** — test today against the current app.
- **🚧 not yet built** — the UI doesn't exist yet (the tab is a stub or the feature is absent).
  These tests are the spec-conformance checklist to run the moment the screen lands; right now
  they document what *should* happen, and the corresponding area must fail gracefully (per §2/§10).

- [ ] = unchecked, [x] = passed. Anything failing should be reproducible from its steps alone.

## 0. Prerequisites & test assets

- The app running on a touch device (Librem 5 / PinePhone) **or** a desktop window at phone width
  (~360–500 px). Desktop is fine for everything except §11's shell/lock-screen tests and §12's
  call tests, which need a real GNOME/phosh session.
- An Audiobookshelf server you control (or the public demo `https://audiobooks.dev/audiobookshelf`,
  username/password `demo`/`demo`).
- A test server set up with:
  1. A **single-file book** (one audio file).
  2. A **multi-file book** with at least 3 audio files ("tracks"), each ≥ 2 minutes, so track
     transitions can be observed without waiting forever.
  3. A book with a **very long title and/or long author name** (for ellipsis checks).
  4. A book you have **partially listened to** on the server (for Continue Listening / Resume).
  5. A book with **many chapters** (20+) and long chapter titles (for the chapter list).
  6. **Two or more libraries** (e.g. one "Books", one "Podcasts") to trigger the library picker
     and per-library behavior.
  7. Optionally: a **reverse proxy in front of the server** (for custom headers / user-agent
     tests) and one serving a **self-signed certificate** (for the TLS tests in §16).
- A way to cut network access (airplane mode / Wi-Fi off) for the offline and adverse sections.
- Optional: a phone able to receive calls (for §12's call-interruption test).

Conventions: **playback position** always means position in the *book*, not in the current audio
file. "Mini bar" is the bottom player strip; "full player" is the Now Playing screen;
"view options sheet" is Library browse's bottom sheet.

## Manual pass results — 2026-10-12 (multi-library, multi-account)

The real 0.9.7 release binary (9e36993), driven under Xvfb with `xdotool`, against two local
Audiobookshelf 2.37.1 servers (Docker, `ghcr.io/advplyr/audiobookshelf`) and the public demo:
- **Server A**: "Audiobooks", "Kids" and "Podcasts"; users `root` and `anna` (Kids only).
- **Server B**: "Fiction"; user `bob`.
- **Demo**: `https://audiobooks.dev/audiobookshelf`.

Each result was checked against the app log and its SQLite database.

**Works:**
- Libraries:
  - Every library on a server is merged into one Library list and one Recently Added shelf.
  - Search spans libraries.
  - A restricted user sees only their libraries.
- Server management:
  - Add Server (with Cancel, playback uninterrupted).
  - Switching servers (about 0.3 s on a local server, player retired with its position saved).
  - Removing a server that isn't active purges its rows, covers and downloads.
- Data kept apart and preserved:
  - Downloads stay per server, including one that finishes in the background after a switch.
  - Progress stays per account.
  - "Log in again" on one server keeps the same account and leaves the others alone.
  - A relaunch with four servers restores the active account without a login.
  - Remembered passwords are removed with their accounts.

**Found** (all fixed on `claude/amazing-knuth-fbmiu5`; verification below):
1. **Serious:** a library deleted on the server makes every later sync of that server fail with
   HTTP 404 for its items. `sync_libraries` never drops libraries the server no longer lists, and
   `sync_all` fetches items for all stored ones. **Fixed:** sync fetches only the libraries the
   server lists and prunes the rest; downloaded books are kept (their library is hidden from
   "Your Libraries" until the downloads are deleted).
2. **Serious:** signing out of, or removing, the *active* account lands on the first-run login
   screen with no Cancel, even with other accounts left. It survives a relaunch. Logging in again
   adds a duplicate server row, and the old row keeps the downloads. **Fixed:** the most recently
   used remaining account becomes active; the login screen without Cancel is only for "no
   accounts left".
3. Adding a server URL that is already in the list creates another row, even for the same user.
   **Fixed:** the same URL (trailing slashes ignored) and user signs that account in again; a row
   left without accounts is reused; a different user still gets a row of their own.
4. Podcasts and e-books are listed like books. Play shows "Connection lost" (real error: no audio
   files). **Fixed:** both are hidden; a podcast library's Home row says "Podcasts aren't
   supported yet"; an item with no audio says "Nothing to play here".
5. "Remove Server" in the server menu is invisible with libadwaita 1.5 (`flat` +
   `destructive-action` gives white on white), though clickable. **Fixed**, and the same for the
   Connection page's "Disconnect from the Server" (found in the single-server pass below).
6. The Library tab doesn't refresh after Home's Sync now or after leaving offline mode. Once,
   after offline mode was turned on and off with a search active, it stayed on "No items yet" with
   24 items stored. **Fixed:** each screen re-reads after the other's sync; leaving offline mode
   syncs; the empty state names its cause ("No matches", "No downloaded items", "No items yet").
7. Smaller issues (all **fixed**):
   - A server added in offline mode says it has no libraries, and leaving offline mode doesn't
     sync it. Now: "Offline mode is on", and turning it off syncs.
   - Continue Listening waits for the next sync. Now: it updates about 2 s after playback stops.
   - An empty search says "Check your connection". Now: "No matches".
   - Running downloads are kept across a server switch and shown again on switching back; Remove
     Server cancels that server's downloads first.

### Fix verification — 2026-10-12

Release build of the fixes, same three-server setup and fresh app data. Every check (R1–R11)
passed, after two fixes made during the pass:
- removing a server that isn't active now stays on Settings (the tab switch ran before the
  rebuilt shell was in place);
- Remove Server during a download no longer leaves an empty download folder behind.

Single-server pass (server A as `root` only, fresh app data): first login, Home and Library
(list/grid, search, Author chip, view options, Sync now), playback controls (±skip, next chapter,
speed, sleep timer, chapters, seek), resume after a relaunch, whole-book download, offline mode
(only downloaded books, playback with the server stopped, progress pushed on leaving it), delete
download + "Download remaining chapters", "Log in again" after the server ended the session (same
account, downloads kept), settings persisting across a relaunch, signing out of the last account
(login screen without Cancel; logging in again reuses the server row and its downloads) and
removing the last server (everything purged). Pull-to-refresh needs touch input, so it was
covered by its automated scenarios. The log showed no new warnings.

Pre-existing, not fixed here:
- Item Detail's download button shows ↓ for a book that is already downloaded, until a download
  event for it arrives (the chapter rows show the checkmarks).
- After a direct shell-to-shell rebuild (switching servers, adding one, a Settings action on a
  server that isn't active), MPRIS fails to register ("An object is already exported"): the new
  shell is built before the old one is released.

## Manual pass results — 2026-09-25

A full manual pass was run against a locally-seeded Audiobookshelf server (Docker,
`advplyr/audiobookshelf`) under Xvfb, driving the real compiled `audiobooklet` binary with `xdotool`
and checking `xdotool getwindowgeometry` after every action.

**One real window-shape bug was found and fixed.** On a genuinely fresh cold launch (no active
account — the Welcome/login screen), the window opened at **398×760**, not its coded 390×760
default; every other screen in this pass (reached by resuming an already-logged-in session, never
by cold-launching straight into Welcome) consistently stayed at exactly 390×760, including through
the deliberate §13 AL resize tests, which behaved correctly and restored cleanly. Root cause: an
unused `.width_request(340)` on the Welcome screen's root `GtkBox` forced GTK's minimum content
width up past what the hero/fields/buttons actually need on that axis; since a window's
`default_width`/`default_height` are only a hint, not a cap, and GTK windows never auto-shrink
once realized, the window grew on its very first show and stayed grown for the rest of the
process — meaning every fresh install or logged-out user would see a permanently 8px-wider app.
Fix: `app/src/screens/welcome.rs` — removed the stray `width_request(340)` (one line; content
already renders identically without it, since the entry rows/buttons naturally fill the same
width). Verified by launching the real binary against a brand-new `$HOME` before and after the
fix (`xdotool getwindowgeometry`: 398×760 → 390×760) and by screenshot comparison (pixel-identical
layout). **No automated regression test was added**: an in-process `measure()` on the unparented
root widget doesn't reproduce the effect (returns a much larger, unrelated natural-size figure),
and presenting the screen in a real `gtk4::Window` in-process didn't reproduce it either (passed
even with the bug artificially reintroduced) — this codebase's GTK scenario-test harness doesn't
faithfully replicate an `AdwApplicationWindow`'s real first-show size negotiation. The fix is
covered by the manual reproduction above instead.

Two environment gaps in the test sandbox were also diagnosed and fixed, but were sandbox
limitations rather than app defects:
- No D-Bus session/system bus, no PulseAudio/PipeWire, and (initially) no `gstreamer1.0-plugins-
  bad`/`gstreamer1.0-libav` meant the app correctly logged and degraded (MPRIS, ModemManager,
  NetworkManager watches, and initial playback all failed cleanly with warnings, no crash) — this
  is §11/§12's territory, expected in a headless container. Installing the missing GStreamer
  plugins and starting a dummy PulseAudio sink for the test `$HOME` fixed real playback, which was
  then verified end-to-end (play/pause, scrubber advancing, skip ±15/30s, speed change, chapters
  sheet, sleep-timer-fires-and-pauses) with zero issues.
- `AdwComboRow`/`GtkPopover` widgets render as separate override-redirect X11 windows that a bare
  Xvfb with no window manager won't deliver synthetic clicks to reliably, **and** `import -window
  <mainwin>` doesn't capture them — capturing `-window root` instead was the fix. Once that was
  understood, LB-6's view-options sheet, the download-scope sheet, the speed/sleep-timer popovers,
  and the theme `AdwComboRow` all showed up and worked correctly; this was a screenshot-tooling
  gap, not an app bug.

**Needs a human on real hardware** (cannot be exercised in any headless container):
- **§11 System media integration (SI-1..SI-5)** — needs a real GNOME/phosh shell (lock screen
  media widget, MPRIS from another app, notification media controls).
- **§12 Hardware controls & interruptions (HW-1..HW-10)** — needs a real phone: physical
  volume/media keys, Bluetooth headset controls, a real incoming call via ModemManager, real
  headphone plug/unplug.
- **AC-5 (screen lock during playback)** — no real compositor/lock screen in this container;
  approximated with an idle wait only, not a true lock.

**Confirmed gracefully absent, as documented:**
- **§4 Library picker (LP)** — not built; login goes straight to the main shell even with 2
  libraries on the seeded server, matching LP-2's fallback expectation. (LP-1/LP-3/LP-4 need the
  picker itself and so remain untestable until it exists.)

**Not exercised this pass** (time-boxed; left `[ ]` with a note at the relevant item rather than
guessed at): §9 Multi-track playback's actual track-boundary crossing (needs sitting through a
full ≥2-minute synthetic track), most of §16 Connection page's TLS/custom-header wire-level
checks (no reverse proxy stood up this pass), and most of §17 Adverse conditions' failure
injection beyond what's noted inline. These are gaps in *pass coverage*, not known failures.

---

## 1. Welcome / Server login (WT) — ✅ implemented

- [x] **WT-1 — First launch shows the login screen.** Fresh app data (or after signing out of
      every server, once Settings exists). Launch the app.
      *Expected:* "Connect to your Audiobookshelf server" screen with icon, description, a
      Password / API Token toggle, Server URL + Username + Password fields, and a **Connect**
      button. No back button anywhere.
      *Note (2026-09-25):* caught a real window-shape bug here — see this doc's dated results
      note after §0. Fixed; re-verified at exactly 390×760 on a brand-new `$HOME`.

- [x] **WT-2 — Connect starts disabled.** Look at the Connect button with an empty form.
      *Expected:* greyed out / unresponsive.

- [x] **WT-3 — Connect enables only when the current mode's fields are filled.** Type into the
      fields one at a time: URL only → still disabled; URL + username → still disabled; then a
      password → enabled. Delete the username → disabled again.
      *Expected:* Connect flips enabled/disabled exactly as described. Whitespace-only input
      counts as empty.

- [x] **WT-4 — Password show/hide.** Tap the eye icon in the Password field.
      *Expected:* password text toggles between dots and plain text.

- [x] **WT-5 — Mode switch swaps fields.** Tap "API Token".
      *Expected:* Username and Password disappear entirely; a single "API Token" field appears.
      Tapping "Password" brings them back.

- [x] **WT-6 — Successful login.** Fill in valid credentials and tap Connect.
      *Expected:* button label changes to "Connecting…" and the form locks during the attempt,
      then the main app shell (Home tab) replaces the login screen.

- [x] **WT-7 — Wrong password error.** Connect with a wrong password.
      *Expected:* the error banner above the form reads "Unable to sign in" / "Check your
      username and password and try again."; the Username **and** Password fields get a red
      error tint; **no** "Details" button (a bad password isn't a transport error).

- [x] **WT-8 — Unreachable server error.** Connect with a valid-looking URL nothing listens on
      (e.g. `http://127.0.0.1:1`), any username/password.
      *Expected:* banner reads "Can't reach this server" / "Check the address and your
      connection."; the Username/Password fields are **not** tinted red (this isn't a
      credentials problem); a "Details" button is available and reveals the raw error text.

- [x] **WT-9 — Timeout error message.** Point the URL at a host that blackholes (e.g. a
      non-routable IP) and connect.
      *Expected:* banner reads "This server took too long to respond — check your connection and
      try again."

- [x] **WT-10 — Form survives a failed attempt.** After WT-7 or WT-8, look at the fields.
      *Expected:* everything you typed is still there, the form is re-enabled, and Connect works
      again without retyping.

- [x] **WT-11 — API token mode is explicitly unsupported.** Switch to API Token, fill URL +
      token, tap Connect.
      *Expected:* a banner explaining that signing in with an API token isn't supported yet —
      use username and password. No crash, no fake login.

- [ ] **WT-12 — Login screen during no network.** With airplane mode on, connect with any
      credentials.
      *Expected:* the connectivity-failure banner (WT-8 wording), app stays on the login screen.

- [ ] **WT-13 — Re-login is pre-filled and abortable.** Trigger the re-login flow (HT-2a: let a
      session die, press Log in again).
      *Expected:* the login screen appears with Server URL and Username already filled in
      (password empty and focused), and a Cancel button below Connect. Cancel returns to the app
      with the old session still working; the form is otherwise the ordinary login screen (WT-1
      minus "no back button anywhere").

- [ ] **WT-14 — Replacing an account asks first.** From WT-13's pre-filled screen, change the
      username (or the URL) and connect.
      *Expected:* a confirmation dialog spelling out what will be removed on this device before
      anything happens. Cancel dismisses it with nothing changed (the form stays usable, the old
      session stays active). Replace proceeds with the login; if the login then fails, the old
      session is still intact (retryable, nothing was removed).

- [ ] **WT-15 — Remember password is on by default and goes to the keyring.** Look at the form,
      then sign in without touching the switch.
      *Expected:* "Remember password" sits under the Password field, switched on, and is hidden in
      API Token mode. After signing in, the keyring (Seahorse, or `secret-tool search xdg:schema
      io.github.gdr_aislop.audiobooklet.Password`) holds one item for the account. If the keyring is
      locked, the system unlock prompt may appear. Signing in again with the switch off, or
      signing out, removes the item.
      *Automated:* `welcome_connect` (switch starts on); abs-core's
      `a_login_remembers_the_password_only_when_asked_to`,
      `a_relogin_that_replaces_the_account_forgets_the_old_password`,
      `signing_out_or_removing_the_server_forgets_the_password`.

---

## 2. App shell & navigation (NT) — ✅ implemented

- [x] **NT-1 — Four tabs exist.** After login, check the bottom tab bar.
      *Expected:* Home, Library, Downloads, Settings — each with an icon and label, Home
      selected.

- [x] **NT-2 — Tab switching.** Tap each tab in turn and back.
      *Expected:* content switches instantly each time; selected tab stays highlighted.

- [x] **NT-4 — Mini bar absent before first playback.** On a fresh session, look above the tab
      bar on every tab.
      *Expected:* no mini-player bar anywhere.

---

## 3. Home (HT) — ✅ implemented

- [x] **HT-1 — Shelves populate.** With a synced server, check Home.
      *Expected:* "Continue Listening" (only books with progress), "Recently Added", and "Your
      Libraries" list. Covers load; items you've started show a "% listened" subtitle.

- [ ] **HT-2 — Empty server state.** Point the app at a server with no libraries / nothing
      synced (or first sync failing).
      *Expected:* while the first sync runs, a spinner with "Syncing your libraries…" instead of
      empty shelves; if the server can't be reached, "Can't reach your server" (within a few
      seconds, never an endless spinner), and for other failures "Couldn't sync your
      libraries" — either with the error in a details line and a Try again button that restarts
      the sync; if the server genuinely has no libraries, "No library synced yet" with Try
      again.

- [ ] **HT-2a — Dead session state.** Point the app at a server that 401s the sync (revoked
      refresh token, removed user — anything that makes the server reject the session).
      *Expected:* "Sign in again" with "Your session on this server has expired or was revoked.",
      the error in the details line, and both Try again and a **Log in again** button. Pressing
      Log in again opens the Welcome screen with URL and username pre-filled (only the password
      left to type) and a Cancel button back to the app.

- [ ] **HT-2b — Re-login replaces cleanly.** From HT-2a's pre-filled Welcome screen, sign in
      with the same credentials; then again with a different account on the same server; then
      with a different server entirely.
      *Expected:* same credentials — straight back to your libraries, no prompt, no re-sync from
      scratch. Different account on the same server — a confirmation dialog explains the old
      account's on-device progress is removed; after confirming, the libraries appear immediately
      (the cache is reused). Different server — the dialog explains the old server's cached data
      is removed; after confirming, a fresh sync runs. Cancelling the dialog or pressing Cancel
      at any point changes nothing (the old session stays intact).

- [ ] **HT-3 — Tapping a cover starts playback.** Tap any cover card in Continue Listening or
      Recently Added.
      *Expected:* playback starts (mini bar appears). Note: the spec's end state is tapping
      through to an Item detail page (§6); until that's built, the card plays directly — that's
      the current intended behavior.

- [ ] **HT-3a — Tapping a shelf heading opens the Library pre-sorted.** Tap the "Recently
      Added" heading.
      *Expected:* switches to the Library tab sorted by date added (newest first), no filter.

- [ ] **HT-3b — Continue Listening heading lands filtered.** Tap the "Continue Listening"
      heading.
      *Expected:* switches to the Library tab sorted by last listened (newest last-listen first,
      never-played books last) and filtered to in-progress books: the filter banner ("Showing
      books in progress", with a Show all button) is visible and the view-options funnel is
      highlighted in the accent colour (it's a funnel either way). Show all (or unticking the dropdown's "In progress only") restores the full library.

- [ ] **HT-3c — "Last listened" sort works from the dropdown.** Open the Library's "Sort &
      filter" dropdown, pick "Last listened"; enable "In progress only" manually.
      *Expected:* books order by when you last listened to them (per the server's own listening
      times); the filter hides finished and never-played books and shows the banner. Neither the
      sort nor the filter survives an app restart (session-transient).

- [ ] **HT-4 — Sync failure banner.** Stop the server (or turn off Wi-Fi) and restart the app.
      *Expected:* **both** Home and Library show the banner "Can't reach your server" /
      "Showing what's saved on this device." with a **Retry** button, and the previously cached
      shelves still render. Retry runs a sync (a "Sync failed" toast while the server is still
      down; the banner goes away once it's back). With nothing cached, Home shows the
      "Can't reach your server" page + Try again instead — the error is never hidden behind an
      empty state or a spinner. If the failure is an authorization failure instead (server 401s
      the session), the banner reads "Signed out by the server" and carries a **Log in again**
      button (HT-2a). No crash, no blank page.

- [ ] **HT-4b — The banner looks right.** With the HT-4 banner showing, in light and dark theme
      on a 360-px-wide phone.
      *Expected:* a rounded, amber-tinted card with clear space on all sides (not text pressed
      into the corner); bold headline, dimmed second line, then "Details" on the left and Retry
      on the right. "Details" opens the raw error underneath; long error text wraps and never
      makes the page scroll sideways.

- [ ] **HT-5 — Cached render beats the network.** With Wi-Fi off but a previously synced Home,
      launch the app.
      *Expected:* shelves appear quickly from cache (possibly followed by the sync-failure
      banner), rather than an empty screen while a network call hangs.

- [x] **HT-6 — Account avatar.** Check the circular avatar button (account's initial letter) at
      the header bar's right.
      *Expected:* a tooltip "Signed in as \<username\>" on long-press/hover. Tapping does nothing
      (account management will live in Settings, §15) — it must not crash.

- [ ] **HT-7 — Horizontal shelf scrolling.** With more items than fit on screen, swipe a shelf
      row sideways.
      *Expected:* the row scrolls horizontally without also scrolling the page vertically.

- [ ] **HT-8 — Offline-mode toggle.** Tap the "Offline" toggle in Home's header
      (leading side, opposite the avatar).
      *Expected:* an "Showing downloaded items only" banner appears below the header and every
      shelf — plus the Libraries list — filters down to downloaded items (fully or partially).
      *Automated:* `home_offline_mode_toggle_filters_recently_added` drives the toggle and the
      shelf filtering; the shared persisted state with Library is asserted there too (the same
      settings key).

- [ ] **HT-9 — Offline toggle is shared state.** With Home's offline toggle on, open Library
      browse; then toggle it there and check Home.
      *Expected:* the same state is reflected on both screens (it's one setting, not two), and
      it survives leaving the tab.
      *Automated:* both screens load/save `abs_core::settings::{load,save}_offline_mode` —
      `home_offline_mode_toggle_filters_recently_added` and
      `library_offline_mode_toggle_filters_to_downloaded_items` each restore the persisted
      value on rebuild.

- [ ] **HT-10 — Sync now.** Home's ⋯ header menu → "Sync now"; or pull the page down past its
      top (pull-to-refresh).
      *Expected:* an immediate re-sync runs — the shelves pick up whatever changed server-side —
      and a transient toast confirms completion — or reports failure — without needing to leave
      the tab. Both triggers share one manual sync path and in-flight guard.
      *Automated:* `home_sync_now_resyncs_and_toasts` (menu → grown mock response + "Sync
      complete" toast), `home_pull_to_refresh_resyncs_and_toasts` (the scroller's own
      `edge-overshot` signal), `home_sync_now_toasts_failure` (unreachable server → "Sync
      failed" toast, empty state unchanged).

---

## 4. Library picker (LP) — 🚧 not yet built

Today every library on a server is merged into Home and Library (see the 2026-10-12 pass).
Podcast libraries are listed under Home's "Your Libraries" as "Podcasts aren't supported yet",
and their episodes, like e-book-only items, are not shown anywhere.

Shown between login and Home only when the server exposes **more than one** library.

- [ ] **LP-1 — Multi-library server shows the picker.** Log into a server with 2+ libraries.
      *Expected:* after login (before the main shell), a simple list with one row per library.

- [ ] **LP-2 — Single-library server skips the picker.** Log into a server with exactly one
      library.
      *Expected:* no picker — straight to the main shell.

- [ ] **LP-3 — Row content.** Inspect the picker rows.
      *Expected:* each row has an icon reflecting the library type (book vs. headphones for
      podcasts), the library name, and the item count as its subtitle.

- [ ] **LP-4 — Selecting a library proceeds.** Tap a library row.
      *Expected:* the main shell opens with that library as the active one (Library browse
      §5 shows its contents).

---

## 5. Library browse (LB) — ✅ implemented

Built: search, sort (incl. "Last listened"), the in-progress filter with its funnel indicator and
banner, grid/list toggle, offline toggle, download badges, Sync now, the view options popover
(Downloaded only/Hide finished/Grouping/Sort by/Application settings), category chips (All/
Author/Series/Genre), and grouped section headers. One documented simplification: LB-9's headers
are *grouped*, not *sticky*-while-scrolling — this crate's rendering model fully rebuilds the grid/
list on every change rather than binding a `GListModel`, which is what real sticky headers need
(`GtkListView` section headers); a plain grouped header is what's actually implemented. The Genre
chip is a session-only filter, not a `Grouping` mode (a book has several genres, not one grouping-
per-genre) — picking a genre reveals a second row of the genres actually present and filters to
one of them, per LB-4 below.

- [x] **LB-1 — Header layout.** Open the Library tab.
      *Expected:* header bar with a **persistent, always-visible search field** (not hidden
      behind a search icon), a filter/sort menu button, and a trailing view-options
      (three-line "adjustments") icon button.

- [x] **LB-2 — Grid of covers.** With content, look at the library.
      *Expected:* a grid of cover art that reflows — the number of columns per row changes as
      width changes (see §13 AL-2), not a fixed column count.

- [ ] **LB-3 — Live search.** Type a few letters of a title into the search field.
      *Expected:* the grid/list narrows live to matching items; clearing the field restores
      everything.

- [x] **LB-4 — Category chips.** Look below the header.
      *Expected:* a horizontally scrollable row of chips: All / Author / Series / Genre. Tapping
      one filters the view; "All" restores it. The chip row scrolls horizontally without
      scrolling the page.

- [x] **LB-5 — Grid/list toggle.** Tap the view-options button and switch between grid and list
      presentation.
      *Expected:* grid shows covers; list shows `AdwActionRow`s (cover thumbnail, title,
      subtitle) — useful for podcast-style episode feeds. Both show the same items.

- [x] **LB-6 — View options sheet opens.** Tap the view-options (funnel) button.
      *Expected:* a bottom sheet slides up with a grip handle (no title), containing: "Downloaded
      only" switch, "Hide finished" switch, "Grouping" combo row, "Sort by" combo row, and an
      "Application settings" row with a chevron.

- [ ] **LB-7 — Downloaded only filter.** In the sheet, turn "Downloaded only" on.
      *Expected:* the grid filters to items downloaded fully or partially. With nothing
      downloaded: a status page "No downloaded items" instead of an empty grid.

- [ ] **LB-8 — Hide finished.** With a fully-listened item in the library, turn "Hide finished"
      on.
      *Expected:* fully-listened items disappear from the grid; turning it off brings them back.

- [ ] **LB-9 — Grouping.** Set Grouping to "By Series", then "By Author", then "None".
      *Expected:* with grouping active, the list sections under headers named for the series/
      author; "None" removes the sectioning. Not sticky-while-scrolling (see this section's
      intro) — a documented simplification, not a bug.

- [x] **LB-10 — Sorting.** Set Sort by to each of: "Date of creation", "Title", "Author",
      "Duration".
      *Expected:* item order visibly re-sorts accordingly each time.

- [x] **LB-11 — Application settings shortcut.** Tap the "Application settings" row in the
      sheet.
      *Expected:* the sheet closes and the app lands on the Settings tab (§15).

- [ ] **LB-12 — Sync now.** Library's ⋯ header menu → "Sync now"; or pull the page down past its
      top (pull-to-refresh).
      *Expected:* immediate re-sync of this library's contents and playback progress; transient
      toast on completion or failure. Same manual path and toast as Home's (HT-10).
      *Automated:* `library_sync_now_and_pull_to_refresh` drives both triggers against stacked
      mock responses (each sync's render is its own observable) and asserts the toast.

- [x] **LB-13 — Download badge on covers.** With items downloaded (§14), look at the grid.
      *Expected:* downloaded items carry a small download badge on their cover.

---

## 6. Item detail (ID) — ✅ implemented

**Correction (2026-09-25 manual pass):** this section's status tag was stale — the Item Detail
page is fully built and was reached directly from Library browse (tap a cover → detail page with
back button, cover, title/author, duration, Play/Resume, download button, and — for a chaptered
book — a Chapters list with a "downloaded" legend). See the dated results note after §0 for what
was and wasn't verified.

- [x] **ID-1 — Reaching the page.** Tap a cover card on Home or in Library browse.
      *Expected:* a detail page pushes with a back button: large cover, title, author/narrator,
      duration, and a progress bar if partially listened.

- [ ] **ID-2 — Back navigation.** Tap the back button.
      *Expected:* returns to exactly where you were (same scroll position in the library).

- [x] **ID-3 — Play vs. Resume.** Compare an unstarted book with a partially-listened one.
      *Expected:* the unstarted book's primary button reads "Play"; the partially-listened one
      reads "Resume" and tapping it continues from the listen position.

- [ ] **ID-4 — Description truncation.** Scroll to a book with a long description.
      *Expected:* truncated with a "more" affordance that expands the full text.

- [x] **ID-5 — Chapter list.** Open a multi-chapter book's detail page.
      *Expected:* chapter rows showing chapter title + duration; the currently-playing chapter
      shows a "currently playing" bars icon; tapping any row seeks playback to that chapter.

- [x] **ID-6 — Download sheet opens.** Tap the download button.
      *Expected:* a bottom sheet titled "Download book" with five rows: "Current chapter",
      "Next chapters" (with a − / count / + stepper), "Remaining chapters", "Entire book", and a
      visually separated destructive "Clear downloaded chapters" row.

- [x] **ID-7 — Size estimates.** Look at each download scope row.
      *Expected:* every scope row shows an estimated size as its subtitle (e.g. "≈340 MB")
      computed from the chapter file sizes in the item's metadata.

- [ ] **ID-8 — Stepper behavior.** Tap the "Next chapters" stepper's − and + buttons.
      *Expected:* count changes by 1 per tap, **default 10**, clamped to the number of chapters
      actually remaining after the current one; both stepper buttons are ≥ 44×44 px and tappable
      independently of the row behind them.

- [ ] **ID-9 — Row activation vs. stepper.** Tap the row body (not the stepper).
      *Expected:* the download starts for the stepper's count. Tapping the other scope rows
      (no stepper) starts their download immediately.

- [ ] **ID-10 — Not enough free space.** With free disk space smaller than a scope's estimate
      (fill the device or use a huge book), open the sheet.
      *Expected:* that row's subtitle switches to a red "Not enough free space" and the download
      cannot be started from that row — instead of failing partway through.

- [ ] **ID-11 — Button state during download.** Start any download and watch the item's
      download button.
      *Expected:* idle download icon → spinner/progress ring while in progress → checkmark once
      at least the current chapter is fully downloaded.

- [ ] **ID-12 — Cancel a download.** While downloading, tap the in-progress button and cancel.
      *Expected:* the transfer aborts; whatever chapters already completed remain valid offline
      content (their rows still show the downloaded glyph).

- [ ] **ID-13 — Downloads are resumable.** Start "Entire book" on a large book, cancel or
      interrupt it (kill app / drop network), then re-tap the same scope.
      *Expected:* the download continues from the last completed chapter — already-downloaded
      chapters are not re-fetched (observable as a much faster second pass).

- [ ] **ID-14 — Clear downloaded chapters.** Tap "Clear downloaded chapters".
      *Expected:* destructive styling; all locally downloaded content for this item is removed;
      the download button returns to idle and chapter rows lose their downloaded glyphs.

- [ ] **ID-15 — Offline-availability marker.** With some chapters downloaded, inspect the
      chapter list.
      *Expected:* each downloaded chapter row shows a small checkmark glyph
      trailing the duration; not-downloaded rows show **no** glyph; the "Chapters" section
      header carries a one-line legend ("● downloaded"); the active chapter additionally shows
      the playing-bars icon.

- [ ] **ID-16 — Insufficient-space row does nothing harmful.** Tap the red "Not enough free
      space" row (from ID-10).
      *Expected:* no download starts, no partial state, no crash.

- [ ] **ID-17 — An interrupted chapter download comes out whole.** Start downloading a single
      large chapter, close the app (or let the phone suspend) halfway through, reopen it and
      download that chapter again until it finishes. Then turn on offline mode and play that
      chapter.
      *Expected:* it plays from the device. The log never shows `streaming a track that has a
      download, which can't be used … downloaded file is N bytes, expected M`. (Before 0.9.3 a
      resume could write part of the file twice, so the finished file was refused.)

- [ ] **ID-18 — A damaged download is noticed and fixed by downloading it again.** Needs a
      download damaged by the bug above (or a downloaded chapter file made bigger by hand, e.g.
      `truncate -s +1M` on it). Play that chapter.
      *Expected:* the log shows `a downloaded file is damaged; it no longer counts as
      downloaded`; the chapter loses its downloaded glyph and the book its downloaded badge;
      downloading it again succeeds from the first byte and it then plays offline. This is
      offline-first: with Wi-Fi off it's noticed just the same, and a download whose file merely
      can't be checked at the moment (as opposed to proven wrong) keeps its glyph and plays.

- [ ] **ID-19 — The cover full-screen.** Tap the cover on a book's page.
      *Expected:* the same viewer as FP-21; closing it returns to the book's page as it was. A
      book without a cover isn't tappable.

---

## 7. Mini player (MP) — ✅ implemented

- [x] **MP-1 — Appears only when playing.** Start playback from Home.
      *Expected:* the mini bar appears above the tab bar with cover thumbnail, title, author,
      play/pause button, and a thin progress line.

- [x] **MP-2 — Visible on every tab.** While playing, cycle through Home / Library / Downloads /
      Settings.
      *Expected:* the mini bar stays put on all of them.

- [ ] **MP-3 — Play/pause from the mini bar.** Tap the mini bar's play/pause button twice.
      *Expected:* audio pauses then resumes; the icon alternates pause/play accordingly.

- [ ] **MP-4 — Progress line moves.** Let playback run.
      *Expected:* the thin line at the bar's bottom edge advances continuously.

- [ ] **MP-5 — Tap opens the full player.** Tap the mini bar itself (not the play button).
      *Expected:* the full Now Playing screen replaces the window content.

- [ ] **MP-6 — 🚧 Swipe-up gesture.** Once the gesture lands (currently tap-only): swipe up on
      the mini bar — carefully, so it doesn't collide with phosh's own bottom-edge gestures.
      *Expected:* the full player opens, same as tapping. On real Librem 5 hardware, confirm the
      hit-zone doesn't fight the shell's overview/app-switcher swipe; tap-to-expand must keep
      working regardless.

- [ ] **MP-7 — State survives open/collapse.** Open the full player, collapse back with the
      down-chevron, open it again.
      *Expected:* position, speed setting, and sleep-timer state are exactly as before — nothing
      resets, playback never stops.

- [ ] **MP-9 — Switching books never plays the old one.** Play book A well into it (ideally a
      multi-file book, an hour or more in). Open book B and tap Play, then *immediately* rewind,
      tap play/pause, and tap the mini bar — all while B is still loading.
      *Expected:* A's audio stops the moment Play is tapped; the mini bar shows B's title and cover
      right away; the full player opens once B has loaded, at B's own position, playing B's audio
      (or paused, if play/pause was tapped an odd number of times while it loaded). A's saved
      position is where it was left. Repeat with a chapter tap on B's detail screen instead of
      Play: B starts at that chapter. The log shows `starting playback`, `resolved; loading` and
      `playback ready` for B, plus `discarded …` lines for anything still in flight for A, and
      never a `track loaded` line for A after B was started.

- [ ] **MP-10 — Pause, then rewind on a stream.** Stream a book (not downloaded), pause, and
      within a second tap skip-back two or three times; also change the speed while paused.
      *Expected:* no "the audio pipeline never actually paused" banner; Play resumes at the new
      position at once (no reload). The log may show `pause confirmation ended by a seek`.

- [ ] **MP-11 — Scrubbing seeks once.** In the full player, drag the scrubber slowly across a
      good part of the current chapter (the bar spans the chapter, not the book — for the
      file-boundary case use a chapter that spans two files, or a multi-file book without
      chapters), then drag it to the far right.
      *Expected:* the time labels follow the thumb while dragging and the thumb isn't pulled back;
      the log shows one `scrub: seeking` and one `seek` line per drag, not dozens. The far-right
      drag lands at the next chapter's start in a mid-book chapter; in the last chapter (or a book
      without chapters) it lands one second before the end and keeps playing — the book is not
      marked finished.

- [ ] **MP-12 — Play on the loaded book.** With a book playing (or paused), open its detail
      screen and tap Play/Resume; then tap one of its chapters.
      *Expected:* no loading state, no gap; the log shows `already loaded; playing` and no
      `starting playback`. The chapter tap jumps there without reloading.

- [ ] **MP-13 — A finished book stays finished.** Use ⋯ → Mark as finished on a playing book,
      then quit and reopen the app; separately, let a book play to its very end and then start
      another one.
      *Expected:* both stay finished (Home, the server's web UI). Play on the finished book starts
      it from the beginning (log: `finished book restarted`).

- [ ] **MP-14 — Losing the stream.** While streaming, turn off Wi-Fi/mobile data for ~30 s and
      back on; separately, pause for more than five minutes (screen off) and press Play.
      *Expected:* the first case logs `stream error; reloading at the same position` once and
      playback continues without a tap (a second loss within a minute shows the banner); the
      second logs `resuming with a fresh connection` and audio starts within a few seconds rather
      than after a long silence.

- [ ] **MP-15 — Speed picked while paused.** Stream a book, pause, pick another speed in the
      full player, wait, then press Play.
      *Expected:* the speed label changes at once; no network activity or stall while paused; the
      audio resumes at the new speed (the log shows one `speed` line, applied at Play).

- [ ] **MP-16 — Quit and signals keep the position.** Play for a minute, then quit with Ctrl+Q;
      separately, play and send `kill <pid>` (SIGTERM) or press Ctrl+C in the launching terminal.
      *Expected:* the log shows `asked to terminate; quitting` for the signals; the next launch
      resumes within a few seconds of where each stopped, and Home shows the same position.

- [ ] **MP-17 — Switching accounts retires the old player.** Play a book, then sign in as another
      account (or sign out). Press a media key or use the lock-screen card.
      *Expected:* the old book stops (its position saved, log: `retired the previous player`);
      the media controls no longer start or move it, and the new shell has nothing playing.

- [ ] **MP-18 — Opening the player while a book loads.** On a slow connection, tap Play on a book
      detail screen.
      *Expected:* the full player opens at once, showing the title and cover with the controls
      greyed out; once the book has loaded they enable and (if you tapped Play) audio starts. Mark
      as finished / Reset progress chosen while it loads take effect once loaded.

- [ ] **MP-19 — Headphone jack bounce.** With "Resume when headphones reconnect" on, unplug the
      headphones (playback pauses), then wiggle the plug so it connects and disconnects quickly.
      *Expected:* playback stays paused until the plug has held for a moment; a firm replug
      resumes it.

- [ ] **MP-20 — Downloaded book on a dead connection.** Download a whole book, then connect to a
      Wi-Fi network that has no internet (or block the server) and press Play.
      *Expected:* it starts within ~3 s from the downloaded files (log: `the server is slow or
      unreachable; starting from the downloaded files`, then `track sources … start_track_local=true`),
      with no stream error. With offline mode switched on it starts at once and the log says
      `offline mode is on and the book is on the device; starting from the downloaded files`.

- [ ] **MP-21 — Partly downloaded book.** Download only some chapters, resume in a chapter that
      isn't downloaded, with the connection dead.
      *Expected:* the log names why the start track is streamed (`not downloaded`, `download not
      finished`, `downloaded file is missing`, …); the failure reads "Can't reach your server"
      (or "Connection lost", if that part had started playing) rather than "Playback stopped
      unexpectedly".

- [ ] **MP-22 — Offline mode is logged.** Toggle offline mode on Home or Library.
      *Expected:* the log shows `offline mode turned on|off` and `playback: offline mode on|off`;
      the setting is also logged at startup (`loaded the offline mode setting`).

- [ ] **MP-23 — The UI stays responsive with a big library.** With a library of a few hundred
      books, open the Library tab, type in search, change the sort, and toggle offline mode.
      *Expected:* the list fills in over a moment instead of freezing the app; scrolling and taps
      stay responsive meanwhile; a render that changes nothing doesn't reset the scroll position.
      The log has no `sqlx::pool::acquire` slow warnings; if the main loop is ever held up it says
      `the main loop was blocked` (with how long) and names the job (`a UI job held up the main
      loop … library render`), and `the database pool is saturated` if the pool itself ever is.

- [ ] **MP-24 — Downloaded badges follow downloads.** Download a book (or a chapter of it) from
      its detail screen, then go back to Home and Library; afterwards clear it from Downloads.
      *Expected:* its cards gain the downloaded badge as soon as the download finishes, and lose
      it once cleared — no restart, sync or offline-mode toggle needed. With offline mode on, the
      book appears in (and leaves) the filtered lists the same way.

- [ ] **MP-25 — Offline mode uses no network.** Turn offline mode on, then: open Home and Library
      (and "Sync now"), open an Item Detail, play and pause a downloaded book, try a download, try
      a book that isn't downloaded. Then turn it off.
      *Expected:* the toggle shows pressed immediately; the log has no network errors or timeouts
      while it's on (`offline mode on: network disabled`, `… not syncing`); "Sync now" and the
      download say offline mode is on; the undownloaded book says it isn't downloaded. Turning it
      off logs `offline mode off: catching up with the server` and the position reaches the server.

- [ ] **MP-26 — Low memory mode.** Settings → Playback → Low memory mode on, then look at Home,
      Library (grid and list), Item Detail, the full player and the mini bar; scroll the Library;
      with burst buffering on, read the burst row. Relaunch the app. Compare `ps -o rss` with the
      mode on and off after scrolling the whole Library.
      *Expected:* every cover is a placeholder at once (nothing else on the cards changes); the
      burst row adds a "Uses up to 64 MB of buffer…" line while both are on (and loses it when
      either goes off); the log shows `low memory mode: opening the database with fewer
      connections…` on the relaunch; turning the mode off brings the covers back; the memory used
      is clearly lower with it on.

- [ ] **MP-27 — The mini bar stands apart from the content.** With a book loaded, look at Home,
      Library (scroll the list so rows run under the bar) and Item Detail, in the light and the
      dark theme.
      *Expected:* the bar is a rounded card with a soft shadow, inset from the screen edges, on a
      strip the same colour as the tab bar; a line separates that strip from the scrolling
      content; list rows never look like they continue into the bar.

- [ ] **MP-28 — The window stays the screen's height after the full player.** Open the full
      player for the book with the longest title (and once while it shows a playback error, e.g.
      with offline mode on for a not-downloaded book), collapse it, and go through Home, Library,
      Downloads and Settings.
      *Expected:* the tab bar stays fully visible above phosh's bottom bar every time; on a short
      screen the player's content scrolls instead.

- [ ] **MP-29 — Play with the server down.** Stop the server (or turn off Wi-Fi), then tap Play
      on a book that isn't downloaded.
      *Expected:* within a few seconds a toast reads "Can't reach server" in full (not cut off)
      with a View button that opens the player, and the mini bar shows ⚠. The full player's banner reads
      "Can't reach your server" / "This book can't start until it's back. Check your connection
      and try again." with Retry — not "Lost the connection while playing" — and shows no
      "0:00 / -0:00" times. Retry with the server back starts the book.

- [ ] **MP-30 — Resume a downloaded book after hours away, on a poor connection.** Leave the app
      closed long enough for the login token to expire (a few hours), then open it on a weak or
      dead connection and Resume a book whose current chapter is downloaded.
      *Expected:* sound within ~3 s of tapping Resume (log: `starting playback`, then `the server
      is slow or unreachable; starting from the downloaded files` within about 3 s). A token
      refresh that fails is tried once, not by every screen in turn: the log shows
      `couldn't refresh the access token` followed by `skipping the token refresh: the last
      attempt failed Ns ago`. The only further attempts are the background retries of MP-31,
      when that line says `unconfirmed=true`.
      *Automated:* `playback_a_downloaded_book_starts_without_waiting_for_a_token_refresh`
      (expired token, a server that never answers `/auth/refresh`); abs-core's
      `a_failed_refresh_is_not_retried_by_the_next_caller`.

- [ ] **MP-31 — The login survives a token refresh whose reply is lost.** With the access token
      expired (open the app after an hour or more away), make the refresh's reply fail to arrive:
      e.g. block the server's responses at the reverse proxy for ~20 s, or drop the connection
      right as the app wakes on a weak network.
      *Expected:* the log shows `couldn't refresh the access token … unconfirmed=true`, then `the
      server may have replaced the refresh token …; retrying while it still accepts the old one`,
      then, once replies get through again, `refreshed the account's access token recovered=true`,
      all within 9 minutes. The app stays signed in (no "Session expired" toast, syncing works),
      and the phone doesn't suspend while the retries run. A slow reply (the server answers, then
      the ~30 KB body trickles in) is waited for, up to 2 minutes, rather than thrown away.
      *Automated:* abs-core's `a_refresh_whose_reply_was_lost_is_retried_with_the_same_token_until_it_lands`,
      `a_rejection_ends_the_retries`, `the_retries_stop_once_the_server_no_longer_accepts_the_old_token`,
      `the_device_is_kept_awake_for_exactly_as_long_as_a_refresh_is_in_flight`; abs-api's
      `refresh_waits_for_a_slow_reply_once_the_server_has_answered`.
      *And when the refresh token is gone for good* (the retries came too late, the app went
      unused past the token's 30-day lifetime, or the server lost its sessions, e.g. after a
      database restore): with a remembered
      password (WT-15) the log shows `signed in again with the remembered password` right after
      the refresh is refused, and the app stays signed in. If the password was changed on the
      server, the log shows `the server refused the remembered password` once, no further sign-in
      attempts follow, and the usual "Session expired — log in again" appears.
      *Automated:* abs-core's `a_rejected_refresh_token_signs_in_again_with_the_remembered_password`,
      `without_a_remembered_password_a_rejected_refresh_token_stays_rejected`,
      `a_password_the_server_refuses_is_not_tried_again`,
      `a_sign_in_that_fails_for_another_reason_is_tried_again_after_the_cool_down`.

- [ ] **MP-8 — Long titles ellipsize.** Play the long-title book and look at the mini bar.
      *Expected:* title/author are single-line with "…" — the bar never grows, wraps, or pushes
      the play button off screen.

---

## 8. Full player (FP) — ✅ implemented

- [ ] **FP-1 — Layout.** Open the full player on a chaptered book.
      *Expected:* "Now Playing" header, down-chevron on the left, ⋯ menu on the right, large
      cover, title, author, the chapter line (`<chapter title> · n of m`, accent colour), scrubber
      with elapsed (left) and remaining (right, "-0:00" style) chapter times and a centred
      "… left in book" label between them, transport row (⏮ previous chapter, skip-back, big
      play/pause, skip-forward, ⏭ next chapter — skip-back/forward round, not stretched into tall
      pills), secondary row (speed, sleep timer, chapters, download).

- [x] **FP-2 — Play/pause.** Tap the big button repeatedly.
      *Expected:* audio toggles; icon alternates between pause and play states.

- [ ] **FP-3 — Skip buttons.** Note the elapsed time, tap skip-back, then skip-forward.
      *Expected:* position jumps by the configured skip interval (per Settings §15) in the right
      directions; audio continues from the new spot; labels update.

- [ ] **FP-3b — Skip during a network stall.** With a *streamed* (not downloaded) book, enable
      airplane mode or throttle Wi-Fi/mobile data, then tap Forward.
      *Expected:* the time label moves to `position + skip interval` and stays there — it must
      never read as, or jump to, the start of the current file/track. If the network is genuinely
      gone, a "Connection lost" banner appears rather than the app silently sitting on a dead
      pipeline; Retry (or Play) resumes from that position once the network is back, without
      needing to restart the app. Regression test for a Librem 5 field report where a second skip
      during a network stall both jumped backwards and left playback unrecoverable.

- [ ] **FP-4 — Scrubber drag.** On a chaptered book, drag the scrubber to the middle of the bar,
      release.
      *Expected:* audio jumps to approximately the middle of the *current chapter* (the bar spans
      the chapter, not the book); elapsed/remaining are chapter-relative and update, and "… left
      in book" changes accordingly; playback continues from there without stalling. Dragging
      while paused also works and playback resumes from the target when played.

- [x] **FP-5 — Scrubber doesn't fight playback.** Don't touch anything for ~30 s.
      *Expected:* the scrubber advances smoothly on its own; it never snaps back or jitters. When
      playback crosses into the next chapter, the knob starts again from the left and the times
      switch to the new chapter.

- [x] **FP-6 — Playback speed.** Tap the speed button (reads "1.0×"), pick 2.0×.
      *Expected:* audio audibly speeds up, the button now reads "2.0×". Available presets:
      0.8×, 1.0×, 1.25×, 1.5×, 1.75×, 2.0×, 2.5×, 3.0×.

- [x] **FP-7 — Speed persists across pause and reopen.** At 2.0×, pause, collapse the player,
      reopen.
      *Expected:* still 2.0×, button label correct.

- [ ] **FP-8 — Sleep timer arms and highlights.** Sleep timer popover → "15 minutes".
      *Expected:* the popover closes and the sleep-timer button gains a visible accent/highlight.
      "Off" removes it.

- [ ] **FP-9 — Sleep timer "End of chapter".** Arm "End of chapter" shortly before the current
      chapter ends.
      *Expected:* playback pauses at (not after) the chapter boundary and the timer disarms
      (highlight gone).

- [ ] **FP-10 — Chapters popover.** Tap the chapters (list) button.
      *Expected:* a scrolling list of chapter titles with durations; the current chapter is
      visibly highlighted; the list reflects position *at the moment you opened it*.

- [ ] **FP-11 — Chapter tap seeks.** Tap a different chapter in the popover.
      *Expected:* popover closes; playback jumps to that chapter's start; reopening the popover
      shows the new chapter highlighted.

- [ ] **FP-12 — Add bookmark.** ⋯ menu → "Add bookmark".
      *Expected:* menu closes and a toast "Bookmark added" appears. (Spec'd bookmark list/UI is
      future work — nothing else is expected yet.)

- [ ] **FP-13 — Mark as finished.** ⋯ menu → "Mark as finished".
      *Expected:* toast "Marked as finished"; the item leaves "Continue Listening" on the next
      Home refresh.

- [ ] **FP-14 — Reset progress.** Note the position, ⋯ menu → "Reset progress".
      *Expected:* toast "Progress reset"; elapsed time drops to (near) zero. No confirmation
      dialog is shown by design — it must still not crash or pause unexpectedly.

- [ ] **FP-15 — Collapse chevron.** Tap the down-chevron.
      *Expected:* returns to whatever tab was open before, mini bar still visible, audio still
      playing.

- [ ] **FP-16 — Next chapter.** On a chaptered book, tap ⏭; then go to the last chapter and tap
      ⏭ again.
      *Expected:* the first tap jumps to the start of the next chapter, the chapter line reads
      "n+1 of m" and the knob is at the left; in the last chapter ⏭ does nothing.

- [ ] **FP-17 — Previous chapter.** Well into a chapter (more than 3 s), tap ⏮; then tap it again
      straight away; then, in the first chapter, tap it once more.
      *Expected:* the first tap goes back to the start of the current chapter; the second (within
      3 s of that start) goes to the start of the previous chapter; in the first chapter ⏮ goes
      to 0:00.

- [ ] **FP-18 — Chapter line.** Watch the chapter line across a chapter boundary; open a book with
      a very long chapter title; tap the chapter line.
      *Expected:* the line follows playback into the next chapter; a long title stays on one line,
      cut off with "…"; tapping the line opens the chapters popover (same as the chapters button).

- [ ] **FP-19 — Book without chapters.** Play a book that has no chapter data.
      *Expected:* no chapter line, no ⏮/⏭ buttons and no "… left in book" label; the scrubber and
      its times span the whole book, as before chapters were tracked.

- [ ] **FP-20 — Fits a phone.** On a 360-px-wide phone (e.g. Librem 5), open the player for a book
      whose title wraps to two lines.
      *Expected:* everything down to and including the secondary row (speed, sleep timer,
      chapters, download) is on screen without scrolling; nothing scrolls sideways.

- [ ] **FP-21 — The cover full-screen.** Tap the Player's cover.
      *Expected:* the cover dims a little under the finger; on release the cover fills the screen
      on black at once (it never opens empty), with ✕ top left and the title at the top. On a
      book opened for the first time a pill at the bottom reads "Downloading full size… N %",
      then "Preparing full size…", and the image sharpens with a short crossfade once the
      original is ready. The log shows `fetched the full-size cover` (its pixel size) and
      `showing the full-size cover` (how long the decode took). Pinch zooms around the fingers up
      to 6×, one finger pans, double-tap toggles fit / 2.5×, Ctrl+scroll zooms on a desktop.
      Zoomed in, fine print on the cover is sharp, not the blur of the 400 px copy. ✕, Escape and
      a swipe down (when not zoomed) bring back the Player as it was; Escape doesn't also close
      the Player. Opening it again is instant and needs no connection.
      *Automated:* `player_screen_tapping_the_cover_opens_the_full_size_original`,
      `player_screen_closing_the_cover_viewer_early_keeps_the_download`; screenshots from
      `player_screen_cover_viewer_screenshots`.

- [ ] **FP-22 — The cover full-screen, offline or in low memory mode.** With Wi-Fi off, open the
      cover of a book whose cover hasn't been opened before; then turn on low memory mode and
      open a cover.
      *Expected:* offline: the small copy, with "Showing a smaller copy — the full-size cover
      needs a connection". Low memory mode: the small copy with "Low memory mode is on — showing
      a smaller copy", zoom stops at 3×, and the server isn't asked for the original.
      *Automated:* `player_screen_the_cover_viewer_in_low_memory_mode_shows_the_small_copy`,
      `item_detail_the_cover_opens_the_viewer_offline`.

---

## 9. Multi-track playback (MU) — ✅ implemented

Use the multi-file book (asset #2). Books split across several audio files must play as one
continuous book.

- [ ] **MU-1 — Continuous playback across files.** Start the multi-file book near the end of
      file 1 and let it run into file 2.
      *Expected:* playback continues into the next file automatically, with no manual action and
      at most a brief gap. The mini bar and scrubber never indicate the book has "ended".

- [ ] **MU-2 — Book-level progress across tracks.** Observe the elapsed/remaining labels and the
      mini bar's progress line before and after a track transition.
      *Expected:* elapsed time keeps counting the *book* position monotonically across the
      transition (e.g. 9:52 → 10:03), it never jumps back to 0:00; remaining time keeps
      shrinking; the mini-bar line keeps advancing.

- [ ] **MU-3 — Only the last track ends the book.** Seek into the last file's final seconds and
      let it finish.
      *Expected:* the book finishes (playback stops / marked finished); the item is treated as
      complete, not left at some mid-book position.

- [ ] **MU-4 — Finishing a middle track doesn't finish the book.** Seek near the end of file 1
      (of 3) and let it run out.
      *Expected:* file 2 starts; the book is **not** marked finished and progress is not 100 %.

- [ ] **MU-5 — Skip across a track boundary.** Position ~5 s before the end of file 1 and tap
      skip-forward (10 s).
      *Expected:* playback lands in file 2 at the correct book position (~5 s in), audio plays
      from there — not an error, not a restart of file 1.

- [ ] **MU-6 — Skip-back across a boundary.** Position ~5 s into file 2 and tap skip-back.
      *Expected:* playback lands near the end of file 1 at the correct book position.

- [ ] **MU-7 — Scrub into a later file.** From early in file 1, open the chapters popover, tap a
      chapter that lives in a later file, then drag the scrubber within it. (For a multi-file book
      without chapters, the bar spans the whole book: drag it to ~60 % instead.)
      *Expected:* the app loads the file containing 60 % and plays from the right spot. This may
      take a moment to load (spinner/silence acceptable), but must end up at the correct
      position and keep playing.

- [ ] **MU-8 — Resume mid-book after relaunch.** While mid-file-2, close the app completely.
      Reopen, go to Home → Continue Listening, tap the book.
      *Expected:* playback resumes at the same book position, which means loading the correct
      file (2) at the correct offset — not restarting from file 1.

- [ ] **MU-9 — Speed re-applies at track change.** Set speed to 2.0× and let a track transition
      happen (or skip across one).
      *Expected:* file 2 also plays at 2.0× — track changes must not reset speed to 1.0×.

- [ ] **MU-10 — Sleep timer across a boundary.** Arm "End of chapter" where the chapter boundary
      coincides with a file boundary.
      *Expected:* playback pauses at that boundary and does not roll on into the next file.

- [ ] **MU-11 — Chapter list stays correct.** Open the chapters popover while in file 2 or 3.
      *Expected:* all book chapters are listed, the current one highlighted, and tapping any
      chapter — including in other files — lands and plays correctly (see also FP-11).

- [ ] **MU-12 — Progress syncs to the server across tracks.** Play across a boundary, then check
      the item's progress on the server web UI (or another client).
      *Expected:* the server shows the book-level position, not a stale or per-file value.

---

## 10. Progressive enhancement / state consistency (PC) — ✅ implemented

- [x] **PC-1 — Position survives tab switching + waiting.** Play something, switch tabs, wait a
      minute, return to the full player.
      *Expected:* scrubber and labels show the advanced position immediately — the full player
      never shows a stale position after reopening.

- [ ] **PC-2 — Rapid play/pause tapping.** Tap play/pause 8–10 times quickly.
      *Expected:* the app stays responsive; final audio state matches the final icon state.

- [ ] **PC-3 — Start playback from Home, immediately open the player.** Tap a cover and
      immediately tap the mini bar.
      *Expected:* the full player shows the right book; no crash even though the first track may
      still be loading.

- [ ] **PC-4 — Switch books mid-playback.** While book A plays, go Home and tap book B.
      *Expected:* book B replaces A cleanly (mini bar updates, position starts at B's resume
      point); no doubled audio, no crash.

- [ ] **PC-5 — Stop at the very start / very end of a book.** Tap skip-back repeatedly at 0:00,
      and skip-forward repeatedly near the end.
      *Expected:* position clamps at 0 and at the book's end; no negative times, no NaN labels,
      no crash.

---

## 11. System media integration (SI) — ✅ implemented

Needs a real GNOME/phosh session.

- [ ] **SI-1 — Shell media card appears.** While playing, open GNOME Shell's quick settings (or
      phosh's lock screen / media widget).
      *Expected:* a media card for "Audiobooklet" showing title, artist/author, cover art and
      a live scrub position.

- [ ] **SI-2 — Shell controls work.** From the shell's media card: pause, play, and drag its
      seek bar.
      *Expected:* the app's audio follows; the mini bar and full player stay in sync with the
      shell's state.

- [ ] **SI-3 — Shell next/previous = chapters.** Use ⏭ / ⏮ on the shell's media card.
      *Expected:* ⏭ goes to the next chapter's start; ⏮ restarts the current chapter, or goes back
      a chapter when pressed within 3 s of its start (the same rules as FP-16/FP-17); the app's
      chapter line follows. For a book without chapters they skip forward/back by the configured
      skip intervals instead. They never jump to another book.

- [ ] **SI-4 — MPRIS metadata updates.** Switch books; check the shell's media card.
      *Expected:* title/author/cover update to the new book immediately, without reopening
      anything.

- [ ] **SI-5 — No duplicate notification.** Check the notification tray while playing.
      *Expected:* the shell media widget is the only playback surface — the app must **not**
      additionally post its own playback notification.

- [ ] **SI-6 — Card controls don't crash the app.** While playing, pause from the lock-screen card,
      then play, ⏭ and ⏮ from it; also run `playerctl pause` / `playerctl play` and press a
      headset's play/pause button.
      *Expected:* each one acts and the app keeps running; the log shows no `RefCell already
      borrowed` panic or abort. Regression test for 0.9.2, where pausing from the phone's media
      widget crashed the app.

- [ ] **SI-7 — Shell's own seek buttons.** On phosh's card, tap ↺10 and ↻30.
      *Expected:* the position moves back 10 s / forward 30 s (phosh's own MPRIS `Seek` amounts,
      independent of the app's Skip settings), and the app's scrubber follows.

---

## 12. Hardware controls & interruptions (HW) — ✅ implemented (needs a phone)

- [ ] **HW-1 — Volume keys.** Press volume up/down during playback.
      *Expected:* they change the **system volume** only — they are never repurposed as
      skip/seek controls.

- [ ] **HW-2 — An incoming call pauses while it rings.** While playing, receive a phone call
      and don't pick up yet.
      *Expected:* playback pauses as soon as the phone starts ringing — not only on pickup.
      The log shows `call_watch`'s "phone call added" and "phone call state state=ringing-in",
      then "phone call event event=Started { incoming: true }", then the player's decision
      ("paused for phone call", or an "ignored:" line naming why — nothing loaded, or already
      paused). No "phone call added" line at all means ModemManager never reported the call
      (check `mmcli -m any --voice-list-calls` while it rings); no "watching ModemManager for
      phone calls" line at startup means the watcher never started (look for its warning).

- [ ] **HW-2b — A rejected or missed call resumes.** While playing, receive a call and reject
      it — then again, and let it ring out (or have the caller hang up).
      *Expected:* playback pauses while ringing and resumes on its own once the call is gone.
      Log: "phone call event event=Ended { answered: false, outgoing: false }" followed by
      "resumed after an unanswered phone call".

- [ ] **HW-3 — No auto-resume after an answered call.** While playing, receive a call, pick
      up, talk, hang up, and wait.
      *Expected:* paused from the first ring, and stays paused after hanging up. Resuming is
      manual (mini bar, player, or shell card). Log: "phone call answered; playback will stay
      paused after it ends", then "phone call ended after being answered; not resuming".

- [ ] **HW-3b — A pause during the ringing is respected.** While playing, receive a call,
      press pause and then play (or just pause) on the lock screen while it rings, then
      reject it.
      *Expected:* rejecting changes nothing — playback stays as you left it. Placing a call
      yourself pauses as it dials and never auto-resumes.

- [ ] **HW-4 — Call watcher is optional.** (If testable) run the app where there is no modem /
      ModemManager (e.g. desktop).
      *Expected:* playback works normally; the app logs a warning at most — the missing watcher
      never blocks or crashes anything.

- [ ] **HW-5 — Unplugging wired headphones pauses.** While playing through wired headphones,
      unplug them.
      *Expected:* playback pauses immediately — audio never continues through the phone's
      speaker. If it doesn't: at the default log level (no `RUST_LOG` needed) the app logs the
      audio server's own port-availability event (`route_watch`, "headphone route changed")
      separately from the player's decision on it (`player`, one of "paused for headphone
      unplug" or an "ignored:" line naming why — the setting is off, nothing is loaded, or it
      was already paused). Seeing the first without the second means the audio server saw the
      unplug but the app didn't act on it — the "ignored:" reason says which; seeing neither
      means the audio server itself never reported the unplug (see HW-6c, and run with
      `RUST_LOG=debug` for the per-scan sink detail behind it). Check the rotating log file
      under `~/.local/state/io.github.gdr_aislop.audiobooklet/logs/audiobooklet.<date>`
      (`abs_storage::AppPaths::logs_dir()`) for a run where stderr wasn't captured. Two more
      things worth checking if "paused for headphone unplug" appears but audio never actually
      stops: a `mpris` "MPRIS command received" line shortly after it means something external
      (a Bluetooth peer, the lock screen's media widget, `playerctl`) sent a `Play`/`PlayPause`
      right back; a later `player` warning that a pause "never reached Paused" means the
      GStreamer pipeline itself got stuck mid-transition — the app recovers from that
      automatically (reloads the track), but it takes a few seconds and is worth reporting if
      it happens routinely on this hardware. With a TRRS headset (inline button or mic), pulling
      the plug also produces an MPRIS `PlayPause` a few milliseconds later — the button contact
      shorting on the way out. Expect it in the log followed by "ignored MPRIS PlayPause right
      after a headphone-unplug pause"; playback must stay paused.

- [ ] **HW-5b — A real play/pause press still works after an unplug.** With a TRRS headset,
      unplug while playing, wait a few seconds, then press play/pause on the lock screen's media
      controls.
      *Expected:* playback resumes (through the speaker). Only a `PlayPause` arriving within
      about a second and a half of the unplug is treated as the plug's spurious button press.

- [ ] **HW-6 — Bluetooth loss pauses.** While playing through Bluetooth headphones, turn the
      headphones off / walk out of range.
      *Expected:* playback pauses.

- [ ] **HW-6b — Wired unplug while Bluetooth keeps streaming does not pause.** With Bluetooth
      headphones connected and playing, plug in and then unplug wired headphones.
      *Expected:* playback carries on through the Bluetooth headphones — an output the listener
      still hears going away is what pauses, not any jack event. Also: changing the volume
      while both are connected must never pause playback.

- [ ] **HW-6c — Audio-server restart is survived.** While the app runs, restart the audio
      server (`systemctl --user restart pulseaudio` on PureOS, `… pipewire pipewire-pulse`
      elsewhere), then repeat HW-5.
      *Expected:* the unplug still pauses. To see what the watcher sees, run the app with
      `RUST_LOG=debug` and watch for `route_watch` lines: one "watching the audio server" per
      connection, one "scanned sinks" per audio-server event listing every sink's headphone
      port availability, and one "headphone route changed" per plug/unplug.

- [ ] **HW-7 — Replug stays paused by default.** With Settings → Playback → "Resume when
      headphones reconnect" off (the default), replug the headphones after HW-5.
      *Expected:* playback stays paused; resuming is manual.

- [ ] **HW-8 — Resume on reconnect (opt-in).** Turn "Resume when headphones reconnect" on,
      play, unplug, replug.
      *Expected:* playback resumes on replug. Then pause manually, replug again — playback must
      **not** resume (a replug only ever undoes an unplug pause, never a manual pause or a
      phone call). The switch is also insensitive while "Pause when headphones disconnect" is
      off.

- [ ] **HW-9 — Unplug behavior is optional.** Turn "Pause when headphones disconnect" off, play,
      unplug.
      *Expected:* playback continues (now through the phone's speaker) — unplugging changes
      nothing.

- [ ] **HW-10 — Headphone watcher is optional.** Run the app where there is no audio server
      reachable via the PulseAudio socket (e.g. a bare sandbox).
      *Expected:* playback works normally; the app logs a warning at most — the missing watcher
      never blocks or crashes anything.

- [ ] **HW-11 — Playback survives screen-off suspend.** Play a *streamed* book, lock the phone
      (screen off), wait 15+ minutes.
      *Expected:* audio keeps playing the whole time — the phone must not suspend while
      something is playing. Verify the app is holding a suspend inhibitor with
      `gdbus call --session --dest org.gnome.SessionManager --object-path /org/gnome/SessionManager --method org.gnome.SessionManager.GetInhibitors`
      while playing (one inhibitor naming this app) and again after pausing (none). Regression
      test for a Librem 5 field report where playback stopped ~10–15 minutes in, coinciding with
      the screen going to sleep.

- [ ] **HW-12 — Wired-headset play/pause button.** Play through wired headphones, press the
      inline play/pause button on the cable.
      *Expected:* playback toggles. If it doesn't, bisect in two steps before filing it as an app
      bug: (1) `busctl --user call org.mpris.MediaPlayer2.audiobooklet /org/mpris/MediaPlayer2
      org.mpris.MediaPlayer2.Player PlayPause` — if this toggles playback, the app's MPRIS side
      works and the gap is in how the shell routes the hardware button to it; (2) `sudo evtest`
      (or `sudo libinput debug-events`) while pressing the button — if no `KEY_PLAYPAUSE`/
      `KEY_MEDIA` event appears at all, the codec driver/firmware isn't reporting the button,
      which is a kernel/firmware issue outside this app.

---

## 13. Adaptive / responsive layout (AL) — 🚧 partially implemented

Only the phone-width single-pane layout exists today; the ≥600 px sidebar is spec'd but not
built (the tests below define the target behavior).

- [ ] **AL-1 — Phone width stays single-pane.** Below 600 px width, look at any screen.
      *Expected:* single-pane navigation with the bottom tab bar; the player takes over the full
      screen.

- [x] **AL-2 — Grid reflow.** In Library browse (once built), slowly drag-resize the window
      across ~300–900 px.
      *Expected:* the cover grid adds/removes columns smoothly to fill the width — never a fixed
      column count, never clipped covers.

- [ ] **AL-3 — Wide-width split view.** 🚧 At ≥ 600 px (desktop window or rotated device).
      *Expected (spec):* the four destinations become a persistent sidebar list with content in
      the second pane; item detail pushes inside the content pane, not full-screen; the mini bar
      sits above the whole split.

- [x] **AL-4 — Width, not orientation, decides.** Rotate a phone to landscape (width > 600 px)
      and back to portrait.
      *Expected:* layout follows the window width exactly — landscape gets the sidebar
      (once AL-3 lands), portrait stays single-pane; no separate orientation logic, no glitches
      on rotation.

- [ ] **AL-5 — Touch targets.** On every screen, tap each interactive element near its edge —
      especially paired/adjacent controls (a row's body vs. its trailing ⋯ menu button; the
      download sheet's stepper).
      *Expected:* every element responds within its own ≥ 44×44 px hit area; no mis-taps
      registering on a neighbor.

---

## 14. Downloads screen (DS) — ✅ implemented (DS-1's storage summary deliberately skipped)

The spec's own "nice to have" — the storage-used/free-space summary row — is the one piece not
built (documented in `app/src/screens/downloads.rs`'s module doc).

- [x] **DS-1 — Summary row.** Open the Downloads tab.
      *Expected:* a grouped list whose first row shows storage used by the app and the device's
      free space.

- [ ] **DS-2 — One row per downloaded item.** Download a couple of books (via §6's sheet).
      *Expected:* an `AdwActionRow` per downloaded item (cover, title), each with a remove
      button.

- [ ] **DS-3 — Remove.** Tap an item's remove button.
      *Expected:* the item's downloaded files are deleted and its row disappears; on the server
      the item itself is untouched (it just loses its offline copy — check its detail page shows
      no downloaded glyphs).

- [ ] **DS-4 — Mid-download row.** While a large download runs, look at the Downloads tab.
      *Expected:* that item's row shows a progress indicator in place of the remove button, plus
      a separate, well-spaced (≥ 44×44 px) stop action; the gray subtitle below the title shows
      live progress — "3/10 chapters · 18.2 MB · 2.1 MB/s" (chapter fraction, bytes, smoothed
      speed) — while the spinner stays as the at-a-glance cue.

- [ ] **DS-5 — Stop keeps completed chapters.** Press Stop on an in-progress download (Downloads
      tab row, or the "Stop download" row in the player's download dropdown while it's running).
      *Expected:* the transfer stops but completed chapters remain valid offline content and the
      row settles into their "N chapters, size" summary — never shown as a failure —
      matching Item detail's resumable behavior.

- [ ] **DS-6 — Empty state.** Remove every download (or fresh install).
      *Expected:* a status page ("No downloads yet"-style), not a blank screen.

- [ ] **DS-7 — Offline playback end-to-end.** Download a book, turn on airplane mode, then play
      it from Continue Listening / Library.
      *Expected:* it plays fully with no network; browsing a downloaded library works offline;
      covers and metadata render from cache. Un-downloaded/streamed items, by contrast, must
      fail gracefully (stall or clear error), never crash.

- [ ] **DS-8 — Progress syncs when back online.** While offline, listen to a downloaded book;
      then restore the network and give the app a moment (or trigger Sync now).
      *Expected:* the position you reached offline appears on the server.

---

## 15. Settings (SE) — 🚧 partially implemented (Account, Servers, Playback, Appearance, Diagnostics, About)

- [x] **SE-1 — Groups exist.** Open the Settings tab.
      *Expected:* `AdwPreferencesPage`-style groups: Account, Servers, Playback, Appearance,
      About. (Only Playback's sleep-timer-default row is still missing.)
      *Automated:* `settings_persistence`, `settings_playback_defaults_theme_and_about` and
      `settings_account_and_servers_rows_reflect_the_database` check the built groups.

- [ ] **SE-2 — Account group.** Inspect it.
      *Expected:* one row for the active server/account (username, server host, "active"
      subtitle) with a chevron; tapping it pushes the active server's Connection page. The
      mockup's "Switch or manage servers" row is deliberately dropped (documented deviation:
      the Servers list is on the same page directly below). There is still **no** top-level
      "Sign Out" button.
      *Automated:* `settings_account_and_servers_rows_reflect_the_database` asserts the row's
      title/subtitle and that tapping it swaps the window's content to the Connection page.

- [ ] **SE-3 — Servers group.** With 2+ configured servers, inspect each row.
      *Expected:* host as title, logged-in username and an "active" marker as subtitle; a
      trailing ⋯ menu button per row (≥44px hit target), clearly separated from the row's own
      tap target.
      *Automated:* `settings_account_and_servers_rows_reflect_the_database` asserts rows,
      subtitles and per-server menu-item sensitivity (Switch insensitive on the active server).

- [ ] **SE-4 — Server row body → Connection.** Tap a server row's body (not its ⋯).
      *Expected:* pushes that server's Connection page (§16) as a window content swap; the
      page's back button restores the shell.
      *Automated:* the push mechanism is exercised via the Account row in
      `settings_account_and_servers_rows_reflect_the_database`; back-restore in
      `connection_page_url_info_disconnect_and_back`.

- [ ] **SE-5 — Switch to this server.** Use a non-active server's ⋯ → "Switch to this server".
      *Expected:* it becomes the active account (its "active" marker moves); the shell is
      rebuilt around the new session (Home/Library re-render with that server's data; ongoing
      playback ends with the old shell).
      *Automated:* `settings_account_and_servers_rows_reflect_the_database` asserts the DB's
      active account flips and the window's content is swapped off the settings screen.

- [ ] **SE-6 — Sign Out (per server).** Use ⋯ → "Sign Out" on a server.
      *Expected:* a confirmation first (local progress is destroyed); confirming removes that
      account's session; if it was the only one, the app returns to the Welcome screen;
      cancelling changes nothing.
      *Automated:* `settings_servers_menu_actions_rebuild_the_shell` drives both the cancelled
      and the confirmed path (content swap to Welcome + DB asserts).

- [ ] **SE-7 — Remove Server.** Use ⋯ → "Remove Server" (destructive styling).
      *Expected:* a confirmation first; the server and its accounts cascade away and its
      on-disk cover/download files are purged; if it was the last one, Welcome appears.
      *Automated:* `settings_servers_menu_actions_rebuild_the_shell` asserts the cascade and
      that a seeded cover file is purged, not orphaned.

- [ ] **SE-8 — Add Server.** Tap the "Add Server" row at the end of the Servers group.
      *Expected:* opens the add-server flow (the Welcome form, with a Cancel affordance back
      to the shell); a successful login adds and activates the new server and rebuilds the
      shell.
      *Automated:* `settings_add_server_row_opens_welcome_and_cancels_back` asserts the content
      swap, the visible Cancel button, and that cancelling restores the exact shell widget; the
      successful-add path rides `welcome_connect` (`add_server_and_login`).

- [ ] **SE-9 — Playback defaults take effect.** Set default speed and skip intervals in
      Settings → Playback, then start a book.
      *Expected:* playback starts at the configured default speed; the player's skip buttons
      jump by the configured intervals (check FP-3 uses these values).
      *Automated:* `settings_playback_defaults_theme_and_about` asserts the live controller and
      persistence; the next-start application rides `start()`'s default-speed parameter.

- [ ] **SE-10 — Sleep-timer default.** Set a sleep-timer default in Settings, then start a book
      and open the sleep-timer popover.
      *Expected:* the default is what the app presents/applies as configured (per spec's
      "sleep-timer default" playback setting). (Row deliberately deferred — nothing consumes the
      value yet.)

- [ ] **SE-11 — Appearance.** Cycle Appearance through Follow system / Light / Dark.
      *Expected:* the whole app (all tabs, player, sheets, popovers) switches theme immediately;
      "Follow system" tracks the OS setting.
      *Automated:* `settings_playback_defaults_theme_and_about` asserts the immediate
      `AdwStyleManager` application and persistence (the "follow system" tracking itself needs
      eyes).

- [ ] **SE-12 — About.** Tap the About row.
      *Expected:* an about dialog with app name, version, website, and license. The version
      matches the app's `--version` output exactly.
      *Automated:* `settings_playback_defaults_theme_and_about` asserts the row opens an
      `AdwAboutWindow`; the version match is guaranteed by both reading `CARGO_PKG_VERSION`.

---

- [ ] **SE-13 — Anonymize logs.** Open Settings → Diagnostics. Run with `RUST_LOG=debug`, point
      the app at an unreachable server (Sign in with a typo'd host) and read
      `~/.local/state/io.github.gdr_aislop.audiobooklet/logs/audiobooklet.*` and the terminal.
      *Expected:* the "Anonymize logs" switch is on by default; the failure lines show `<url>` /
      `<server>` / `<user>` and never the host, username or a `token=` value. Turn the switch
      off and retry: real addresses appear. Restart: the switch keeps its state.
      *Automated:* `settings_anonymize_logs_switch_drives_the_scrubber_and_persists`; the
      scrubber itself by `log_privacy::tests` and `crash_reporting::tests`.

- [ ] **SE-14 — Open latest log file.** In Settings → Diagnostics, tap "Open latest log file"
      right after launching the app.
      *Expected:* the newest `logs/audiobooklet.<date>` opens in the default text viewer and already
      contains this session's startup lines (the log is flushed first). With the logs directory
      emptied beforehand, a "No log file yet" toast appears instead; with no text handler
      installed, an "Opening the log file failed" toast.
      *Automated:* `settings_open_latest_log_row_reports_a_missing_log`;
      `AppPaths::latest_log_file` by `abs-storage` `paths` tests.

## 16. Connection page (CN) — ✅ implemented (Server connection row, Advanced group, Disconnect)

Reached from Settings → a server row's body. One instance per configured server. Shown as a
window content swap (no `AdwNavigationView` at the libadwaita `v1_2` ceiling) with a back
button restoring the shell.

- [ ] **CN-1 — Server connection row.** Open a server's Connection page.
      *Expected:* a "Server connection" group with one row whose subtitle is the server's full
      URL in a monospace font, plus a trailing info button whose popover explains what this
      connection is used for.
      *Automated:* `connection_page_url_info_disconnect_and_back` asserts title/subtitle, the
      monospace stylesheet flag, and the popover opening.

- [ ] **CN-2 — Advanced group present.** Open the group below the Server connection row.
      *Expected:* rows for Custom Headers, Disable SSL verification (switch), Client
      certificate, Local network server address, and Change User Agent — chevron rows with
      value-as-subtitle ("None"/"Default" when unset) except the switch; each opens an editor
      dialog that validates inline and persists to the server's row on Save.
      *Automated:* `connection_advanced_rows_persist_and_edit` drives all five rows (initial
      subtitles, the SSL switch persisting, valid + invalid header saves, the no-file cert
      save error, user-agent set/clear, and each row's subtitle following the database).

- [ ] **CN-3 — SSL verification off by default.** Check the switch.
      *Expected:* off; toggling it on persists per-server and takes effect without
      reinstalling.
      *Automated:* the switch's initial state and immediate persistence in
      `connection_advanced_rows_persist_and_edit`; the wire-level effect in abs-api's
      `#[ignore]`d live tests against self-signed.badssl.com (verification off connects, on
      fails).

- [ ] **CN-4 — Self-signed cert flow.** Point a server at a self-signed host. With verification
      on, connect; then enable "Disable SSL verification" and connect again.
      *Expected:* fails with the TLS error message while verification is on ("Can't verify this
      server's certificate…" — see WT-8's family); succeeds once verification is disabled.

- [ ] **CN-5 — Custom headers.** Add a request header (e.g. `X-Auth: secret`) via the Custom
      Headers page, with a logging proxy in front of the server.
      *Expected:* the header appears on every request the app sends to that server (sync,
      downloads, playback metadata, token refresh); removing it stops it being sent. Invalid
      input (no colon, a bad name/value, or `authorization`) is rejected inline and changes
      nothing.
      *Automated:* the editor's valid/invalid/rejected-`authorization` paths in
      `connection_advanced_rows_persist_and_edit`; the header reaching the wire (and token
      refresh specifically) in abs-api's and abs-core's mocked tests.

- [ ] **CN-6 — User-Agent override.** Set a custom User-Agent and check the proxy/server logs.
      *Expected:* before any override, API calls, token refreshes and audio stream requests all
      show `Audiobooklet/<version> (Linux)` in the server's access log (not `-`, not GStreamer's
      agent). With an override they carry the overridden value; clearing it restores the default.
      *Automated:* the user-agent editor's set/clear paths in
      `connection_advanced_rows_persist_and_edit`; abs-api's
      `requests_send_the_app_s_own_user_agent_unless_overridden` and
      `custom_headers_and_user_agent_reach_the_wire`; abs-core's connection tests for the agent
      playback is given.

- [ ] **CN-7 — Client certificate (mTLS).** Import a client certificate for a server that
      requires one.
      *Expected:* connection succeeds only with the cert; the page shows the imported cert's
      file name as the row subtitle, and Remove Certificate clears it. Applies to reqwest
      traffic (sync/downloads/refresh) — playback via GStreamer is a documented gap. A save
      with no file chosen is an inline error.
      *Automated:* the no-file save error in `connection_advanced_rows_persist_and_edit` and
      the same read+parse at save and mint time in abs-core/abs-api's tests (a real .p12 can't
      be produced hermetically; the chooser itself stays manual).

- [ ] **CN-8 — Local network address.** Set a LAN address (e.g. `http://192.168.1.50:13378`) and
      join the server's home Wi-Fi.
      *Expected:* the app talks to the LAN address (visible in server logs as the LAN IP),
      avoiding the public route; off that network it falls back to the public URL automatically
      (a short reachability probe, cached per session for a minute). Stream URLs follow the
      same resolution.
      *Automated:* the editor's normalize/persist path in
      `connection_advanced_rows_persist_and_edit`; the prefer-when-reachable/fallback policy,
      the probe itself, and track-URL resolution in abs-core's `connection` tests.

- [ ] **CN-9 — Disconnect from the Server.** Tap the destructive "Disconnect from the Server"
      action at the bottom.
      *Expected:* flat/link destructive styling (not a filled button); a confirmation first
      (local progress is destroyed); disconnecting signs the server's account out and returns
      appropriately (Welcome when no active account is left) without crashing; other servers are
      untouched.
      *Automated:* `connection_page_url_info_disconnect_and_back` drives both the cancelled and
      the confirmed path, plus the back button's shell restore.

---

## 17. Adverse conditions (AC) — ✅ testable today

- [ ] **AC-1 — Network drops mid-stream.** While streaming a book, disable Wi-Fi/data.
      *Expected:* the app must not crash, hang, or show garbage. Playback may stall/retry;
      controls stay responsive; the last known position is kept.
- [ ] **AC-2 — Network returns.** Re-enable the network after AC-1 and interact with the app
      (switch tabs to force re-sync).
      *Expected:* the app recovers: Home re-syncs, the banner (if shown) clears, and starting
      playback works again without restarting the app.
- [ ] **AC-3 — Server dies mid-session.** Stop the Audiobookshelf server process while the app
      is playing from it.
      *Expected:* as AC-1 — graceful stall or stop, no crash, controls responsive.
- [ ] **AC-4 — Kill the app during playback.** Force-close the app mid-book, relaunch.
      *Expected:* app opens to the main shell (or login if no server); Home's Continue Listening
      shows the last synced position; tapping the book resumes near where you were (within the
      last-synced tolerance).
- [ ] **AC-5 — Screen lock / blank during playback.** Let the screen blank or lock the device
      while playing.
      *Expected:* audio keeps playing (it's an audiobook app); on unlock, the UI shows the
      advanced position.
- [ ] **AC-6 — Rotation / window resize during playback.** Rotate the device (or resize the
      desktop window) while the full player is open, across the 600 px line and back.
      *Expected:* layout reflows without losing state; playback continues. (Spec'd wide-mode
      sidebar is not built yet — see §13 — so single-pane layout at every width is currently
      correct, as long as nothing overlaps or crashes.)
- [ ] **AC-7 — Very narrow window.** Squeeze the window to ~360 px or the device's minimum.
      *Expected:* everything stays usable: labels ellipsize, buttons remain tappable, nothing
      overlaps or clips.
- [ ] **AC-8 — Popover dismissal.** Open speed, sleep-timer, chapters and ⋯ popovers; tap
      outside each one.
      *Expected:* each popover closes; nothing lingers over the transport controls.
- [ ] **AC-9 — Rapid navigation during sync.** On a slow network, launch the app and
      immediately tap through all tabs while Home is still syncing.
      *Expected:* no crash, no duplicated shelves; when sync finishes, Home renders correctly
      whenever you look at it next.
- [ ] **AC-10 — Login with weird URLs.** Try `https://server` (no path), a URL with a trailing
      slash, a URL with spaces, and a completely invalid string.
      *Expected:* no crash anywhere; a clear failure banner for the invalid ones; the valid
      variants either work or fail with the connectivity message — never a silent freeze.
- [ ] **AC-11 — Unicode / odd metadata.** Play a book whose title/author contain emoji, CJK
      characters, or very long words.
      *Expected:* labels wrap or ellipsize cleanly in the mini bar and full player; the avatar
      initial still renders.
- [ ] **AC-12 — Storage pressure.** Fill the device until free space is below the size of the
      next chapter being downloaded (see ID-10).
      *Expected:* the UI flags it before starting ("Not enough free space"); a download that
      *does* run out of space mid-way stops cleanly, keeps completed chapters, and surfaces an
      error — never corrupts existing downloads or crashes.
- [ ] **AC-13 — Offline start of a fully-downloaded book.** With the device offline (or the
      server unreachable), tap a fully-downloaded book to start playback.
      *Expected:* playback starts normally from the local files — book duration and position
      come from locally cached metadata, and no request reaches the server.
- [ ] **AC-14 — Offline start of a partially-downloaded book.** With the device offline, tap a
      book with only some chapters downloaded, positioned inside (or before) a downloaded one.
      *Expected:* playback starts from the downloaded content and stops cleanly when it crosses
      into a missing chapter (treated like a pause, not an error); a book with no cached
      metadata at all (never synced on this device) simply doesn't start.

---

## Regression quick-pass (15 minutes)

When touching the player or sync code, at minimum re-run: WT-6, NT-2, HT-3, MP-1–MP-5, FP-2,
FP-4, FP-6, FP-11, MU-1, MU-5, MU-8, PC-4, AC-1.
