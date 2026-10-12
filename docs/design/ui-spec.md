# UI/UX Design Spec — Audiobooklet, a client for Audiobookshelf servers on mobile Linux (phosh) / GTK4 + libadwaita

This document defines the interface for a native client for Audiobookshelf servers targeting mobile Linux
(phosh, e.g. Librem 5 / PinePhone) and desktop GNOME, built with GTK4 + libadwaita in Rust.

Feature scope and information architecture are informed by [Lissen](https://lissenapp.org/), an
Audiobookshelf client for Android. The visual language, navigation patterns, and widget choices
follow the GNOME HIG and adaptive-app conventions (as seen in apps like Podcasts, Decibels, and
Fractal) rather than copying Lissen's Material Design look.

## 1. Screen inventory

| Screen | Purpose |
|---|---|
| **Welcome / Server login** | Enter Audiobookshelf server URL, authenticate (username+password or API token). |
| **Library picker** | Choose which library (books / podcasts) to browse, if the server has more than one. |
| **Home** | "Continue listening" shelf, "Recently added", quick access to libraries. |
| **Library browse** | Grid/list of items in a library; filter by author, series, genre; sort; search. |
| **Item detail** | Cover, metadata, description, chapter/episode list, play/download actions. |
| **Player (mini)** | Persistent bottom bar: cover thumbnail, title, play/pause, progress line. |
| **Player (full)** | Now-playing page: large cover, scrubber, chapter list, speed, sleep timer, skip ±. |
| **Downloads** | Manage items downloaded for offline playback; storage usage; remove. |
| **Settings** | Account/server management, playback defaults (speed, skip interval), appearance, about. |
| **Connection** | Per-server advanced connection settings: headers, TLS, client cert, local address, user agent. |

## 2. Navigation model

- **Phone width (~360–599px, single pane):** `AdwViewStack` + bottom `AdwViewSwitcher`-style tab
  bar with 4 destinations: **Home**, **Library**, **Downloads**, **Settings**. Each destination is
  an `AdwNavigationView` stack so drilling into an item (e.g. Library → Item detail) pushes a page
  with a back button, per GNOME HIG.
- **Wide width (≥600px, tablet/desktop):** `AdwNavigationSplitView` — the same 4 destinations
  become a persistent sidebar (`AdwViewStackSidebar`-like list), with content shown in the second
  pane. Item detail pushes within the content pane, not full-screen.
- The breakpoint is implemented with `AdwBreakpoint` at 600px width, matching libadwaita's own
  convention for phone-vs-wide layouts (same threshold `AdwNavigationSplitView` uses natively).
- **Player** is modeled as an overlay, not a 5th tab:
  - A **mini-player bar** is pinned above the bottom tab bar (phone) or above the sidebar/content
    split (wide) whenever something is loaded, via `AdwToolbarView`'s bottom bar slot.
  - Tapping the mini-player pushes the **full player** as a modal `AdwNavigationPage` (phone) or a
    `AdwDialog` sized to the window (wide), swipeable/dismissible back down to the mini-player.
  - This mirrors Lissen's mini-player → full-player expansion, which is a near-universal pattern
    for audio apps and translates directly to libadwaita idioms.

## 3. Screen-by-screen detail

### Welcome / Server login
- `AdwStatusPage`-style centered layout: app icon, "Connect to your Audiobookshelf server" title,
  a one-line description. No header-bar back button — this is the first thing a user with no
  configured server sees, so there's nothing to navigate back to.
- A segmented toggle (`AdwToggleGroup`/two-button segmented control, matching the same pattern
  used for Settings' Theme picker) switches the form between two auth modes:
  - **Password** — Server URL, Username, and a masked Password field (with a show/hide eye-icon
    toggle) as one grouped card of `AdwEntryRow`/`AdwPasswordEntryRow`-equivalent rows, then a
    **Remember password** row with a switch, **on by default** (subtitle "Signs you in again if
    the server ends the session. Kept in your keyring."). A remembered password lives in the
    desktop keyring (never the app's database). When the server rejects the refresh token for
    good, the app signs in again with it silently, so one sign-in lasts however long the app goes
    unused. Signing in with the switch off, signing out, or removing the server forgets it.
  - **API Token** — Server URL and a single Token field; username/password are hidden entirely
    rather than just disabled, since they're not applicable to this mode.
- Primary `AdwButton` (suggested-action style, full width) "Connect", **disabled until the
  required fields for the current mode are filled in** — on first launch, with an empty form,
  this is the state the user actually sees.
- **On-screen keyboard**: the Server URL field sets `GtkEditable:input-purpose` to `URL` (via its
  internal `GtkText` delegate, which `AdwEntryRow` doesn't expose) so mobile keyboards show their
  URL layout — dedicated `/` and `:` keys, no autocapitalization. The same applies to the
  Connection page's Local Network Server Address editor; the other fields (username, password,
  token) stay plain-text/purpose-managed.
- **Error state**: on a failed connection attempt, an inline `AdwBanner`-style strip appears above
  the form, with copy that distinguishes *why* it failed rather than one generic message:
  - **Authentication failure** (bad credentials/token): "Unable to sign in" / "Check your
    username and password and try again.", and the offending field's (username/password/token) label
    switches to an error/red tint.
  - **Connectivity failure** (unreachable host, DNS failure, TLS/certificate error, timeout):
    "Can't reach this server" / "Check the address and your connection.", with no field-level red tint,
    since this isn't a credentials problem and tinting the password field would misdirect the
    user. Self-hosted Audiobookshelf setups hit this case at least as often as bad passwords
    (wrong port, VPN not up, LAN-only address used off-network), so conflating the two would
    send users fixing the wrong thing.
  The form retains whatever the user typed in either case — a failed attempt should never clear
  the fields.
- This screen (via `abs_core::accounts::add_server_and_login`) is what's shown whenever
  `abs_storage::repo::accounts::get_active` returns `None` — i.e. on first launch, or after
  signing out of every configured server.
- **Re-login mode** (launched from a dead session's "Log in again", see the flow above): the
  form is pre-filled with the previous session's URL and username and focuses the password field;
  a Cancel button appears below Connect; and connecting with credentials that would replace the
  previous session's local data asks for confirmation first (a different account, or a different
  server — same credentials just rotate tokens with no prompt).

### Library picker
- Shown only if the server exposes >1 library. Simple `AdwActionRow` list, one row per library
  with an icon (book vs. headphones for podcasts) and item count as subtitle.

### Home
- `AdwToolbarView` with `AdwHeaderBar` (title "Home", avatar/account button on the right).
- Horizontally-scrolling `GtkListView`/carousel rows: "Continue listening" (thin progress bar with
  percentage under the cover), "Recently added". Each cover is a tappable card pushing Item detail.
- **Shelf headings are tappable** (tap-through): "Recently added" opens the Library tab pre-sorted
  by date added; "Continue listening" opens it pre-sorted by **last listened** (newest last-listen
  first) and filtered to **in-progress** books — the Library's own "Sort & filter" dropdown and
  banner reflect that state, and both are session-transient (navigation-with-intent, not a
  preference).
- Empty/offline states. While nothing is cached locally, Home shows a full-screen status view
  whose content follows the sync lifecycle rather than a single static message — a fresh login
  with the sync still running in the background must not read as a dead end:
  - **Syncing** (no cached data, sync in flight): an indeterminate `GtkSpinner` above the title
    "Syncing your libraries…", subline "This can take a moment on first sync." Animation-only
    feedback; the sync pipeline doesn't report per-step progress.
  - **Failed** (sync errored, nothing cached): when the server never answered (refused,
    unreachable, no reply in time), a no-network glyph, "Can't reach your server", "Check your
    connection, or that the server is running, then try again."; for any other failure, a
    warning glyph, "Couldn't sync your libraries", "Check your connection and try again." Either
    way, the underlying error text in a small selectable details line, and a **Try again** button
    that re-runs the sync cycle (returning to the Syncing state first).
  - **Signed out** (authorization failure — 401/403 from any authenticated call, meaning the
    stored session itself is dead: expired or revoked refresh token, removed or demoted user):
    a lock glyph, "Sign in again", "Your session on this server has expired or was revoked.",
    the underlying error text in the same selectable details line, and two buttons — **Log in
    again** (routes to the Welcome screen, pre-filled with this session's URL and username, per
    the re-login flow below) alongside Try again. A retry cannot fix a dead session, so the
    login affordance is the point; both buttons stay up since transient misconfigurations do
    happen on self-hosted servers.
  - **Empty** (sync succeeded but the server exposes no libraries): the folder-music glyph,
    "No library synced yet", "This server doesn't have any libraries yet.", and Try again.
  Once anything is cached, shelves render normally and a failed re-sync surfaces as the error
  banner instead (see "Error/offline" under states for its look): "Can't reach your server" (or
  "Couldn't update your library" when the server answered with an error), "Showing what's saved
  on this device.", and a **Retry** button that runs the same sync as Sync now. It sits below
  the header bar, outside the shelves' scroller, so it's visible in every state. On an
  authorization failure it reads "Signed out by the server" and its button is **Log in again**.
  Home and Library share one startup sync (only one of them runs it); the other is told how it
  ended, so both show the same banner or failure state — never a silent redraw, or a spinner
  that never ends.

### Re-login flow
- When a session dies (the Signed-out state above), "Log in again" swaps the window's content for
  the Welcome screen in its re-login mode: the form comes **pre-filled with the previous
  session's URL and username** (the session died, not the server address — only the password
  should be left to type), the password field takes focus, and a **Cancel** button returns to the
  main window with the old session untouched. Cancel exists so "Log in again" is never a trap.
- Signing in with the **same URL and username** rotates the stored tokens in place — nothing else
  changes, all cached data is kept.
- Signing in with **different credentials** — a different account on the same server, or a
  different server — first asks for confirmation (a modal `GtkMessageDialog`, since this crate's
  libadwaita ceiling predates `AdwAlertDialog`), spelling out what is replaced: a different
  account on the same server keeps the server's cached libraries/items/covers/downloads (they're
  keyed by server) but removes the old account's on-device progress; a different
  server removes everything cached for the old server, rows and on-disk files alike. Nothing is
  removed until the new login actually succeeds — a failed login leaves the old session intact.
  Both ways, everything that matters stays on the server; only the local cache is touched.
- **Offline-mode toggle**: a `GtkToggleButton` in the header bar (leading side, opposite the
  avatar), iconified with an airplane/cloud-off glyph and labeled "Offline". When active, an
  `AdwBanner`-style strip appears below the header ("Showing downloaded items only") and every
  shelf (Continue listening, Recently added) and the Libraries list filter down to items that are
  downloaded fully or partially — the same downloaded-item definition used everywhere else in this
  spec (see Item detail). This is state shared with the equivalent toggle on Library browse, not a
  per-screen setting.
  **Offline mode also cuts all network use** (see "Offline-first playback"). The toggle reacts at
  once; the shelves re-filter right after.
- **Sync now**: a header-bar menu item (in the same `⋯` overflow as any other page-level actions)
  that forces an immediate resync of library contents and playback progress with the server,
  rather than waiting on whatever background sync interval is configured. Self-hosted servers on
  home networks are more likely to have stale or interrupted background syncs than a hosted
  service would, so this needs to be a user-triggerable action, not just an automatic behavior.
  Shows a transient `AdwToast` on completion or failure. Shared with Library browse (see below).
  A **pull-down-to-refresh** gesture on the page's main scroller runs the exact same manual path —
  hand-rolled via `GtkScrolledWindow`'s `edge-overshot` signal, since neither GTK4 nor libadwaita
  ships the gesture (and the GNOME HIG doesn't prescribe one); the threshold-less overshot trigger
  needs the same on-device calibration as the mini-player's swipe-up gesture above.

### Library browse
- Header bar with a **persistent, always-visible `GtkSearchEntry`** — not revealed behind a search
  button — since search is a high-frequency action in a large library and worth the permanent
  header-bar space on a device where reaching a reveal-then-tap-then-type sequence one-handed is
  already awkward. Also a **"Sort & filter" `GtkMenuButton`** — one popover, two caption-labelled
  sections: **Sort** (Date added / Title / Author / Duration / **Last listened**) and **Filter**
  ("In progress only" check). The header button always shows a funnel (Adwaita has no funnel, so
  the app ships `funnel-symbolic` itself, compiled into the binary — see `app/src/icons.rs`); while
  a filter is active the funnel is drawn in the accent colour, and an in-view banner ("Showing books in progress ·
  **Show all**") is the ambient indicator plus one-tap clear — same placement/idiom as the offline
  banner. Also a trailing **view-options button**
  (the funnel) that opens the view options sheet described below; these two stay
  compact icon-buttons since they're used less often than search.
- **Sync now**: same header-bar `⋯` menu item as Home (see above), forcing an immediate resync of
  this library's contents and the user's playback progress — plus the same pull-to-refresh gesture
  on this screen's main scroller, both sharing one manual sync path and outcome toast with Home's.
- Content: `GtkGridView` of cover art (grid mode) or `GtkListView` with `AdwActionRow`s (list mode,
  useful for podcast episode-style feeds); toggle between the two via header bar button.
- Sticky section headers when sorted/grouped by author or series (`GtkListView` section headers).
- Category chips (All/Author/Series/Genre) stay in a scrollable row below the header for quick
  filtering, unchanged.
- **View options sheet**: an `AdwBottomSheet` (grip handle, no title needed — the rows are
  self-explanatory) with:
  - **Downloaded only** — `AdwSwitchRow`. When on, the grid filters to items downloaded fully or
    partially, matching Home's offline-mode behavior (see below) but scoped to this library; an
    empty result shows `AdwStatusPage` ("No downloaded items").
  - **Hide finished** — `AdwSwitchRow`, filters out fully-listened items.
  - **Grouping** — `AdwComboRow` (e.g. "By Series", "By Author", "None").
  - **Sort by** — `AdwComboRow` (e.g. "Date of creation", "Title", "Author", "Duration").
  - **Application settings** — plain `AdwActionRow` with a chevron, navigating to the Settings tab;
    included here because it's where Lissen places it and it's a reasonable one-tap shortcut from
    the library a user spends most of their time in.
  All rows use native libadwaita controls (real `GtkSwitch`-style pill toggles, not a
  platform-specific switch skin) and the sheet itself is the same `AdwBottomSheet` pattern used for
  Item detail's download sheet, so the two don't feel like different UI systems.
- **Touch targets**: the 44×44px minimum (§ 4) applies per interactive element, not per control
  group — this matters for any compact paired control like the download sheet's chapter-count
  stepper (see Item detail) and for adjacent tap zones like a row's body vs. its trailing `⋯` menu
  button (see Downloads, Settings' Servers group): each needs its own full-size hit target with
  enough spacing from its neighbor to avoid mis-taps.

### Item detail
- `AdwNavigationPage` pushed from Home/Library.
- Top: large cover art, title, author/narrator, duration, progress bar if partially listened.
  Tapping the cover opens it full-screen (see **Cover viewer** under Player — full).
- Actions row: primary "Play"/"Resume" button, secondary download button.
- **Download options (Lissen-style)**: the download button opens an `AdwBottomSheet` titled
  "Download book" (libadwaita's adaptive bottom-sheet widget, matching Lissen's own sheet) instead
  of immediately downloading everything:
  - **Current chapter** — just the chapter currently playing/at the last playback position.
  - **Next chapters** — an inline numeric stepper (`−` / count / `+`, each button ≥44×44px per
    the touch-target note above) on this row lets the user pick exactly how many upcoming
    chapters to fetch, defaulting to 10 and clamped to the number of chapters actually remaining
    after the current one. Tapping the row (outside the stepper) starts the download for that
    many chapters; the other rows have no stepper and act immediately on tap.
  - **Remaining chapters** — from the current position to the end of the book.
  - **Entire book** — all chapters, regardless of playback position.
  - **Clear downloaded chapters** — a destructive row, separated from the four download options,
    that removes whatever has been downloaded locally for this item.

  Each of the four scope rows shows an **estimated download size** as its subtitle (e.g.
  "≈340 MB"), computed from the chapter file sizes already present in the item's metadata, so the
  user can make an informed choice before committing — important on a device with limited local
  storage. If an option's estimated size exceeds current free space, that row's subtitle switches
  to an error tint reading "Not enough free space" instead of letting the download start and fail
  partway through.

  **Downloads are resumable**: if a download is interrupted (connectivity loss, app closed),
  re-tapping the same scope option continues from the last completed chapter/byte rather than
  restarting from scratch. While a download is in progress, the download button's in-progress
  state (spinner/progress ring) is tappable to reveal a **cancel** action that aborts it, leaving
  whatever chapters completed so far in place as valid offline content.

  Once a download starts, the download button itself reflects overall state for the item (idle
  download icon → in-progress spinner or progress ring → checkmark once at least the current
  chapter is fully downloaded), mirroring the badge already used on Library covers.
- `AdwExpanderRow` or plain text block for description (truncated with "more").
- Chapter/episode list as `AdwActionRow`s, each showing chapter title + duration, tap to seek.
- **Offline-availability marker**: each chapter row that has been downloaded shows a small
  checkmark glyph (`object-select-symbolic`) trailing the duration, distinct from the "currently playing"
  bars icon already used on the active chapter. Chapters not yet downloaded show no glyph at all —
  presence of the glyph is the signal, so the list isn't cluttered with a "not downloaded" icon on
  every other row. A one-line legend ("● downloaded") sits in the "Chapters" section header so the
  glyph's meaning doesn't need to be inferred.

### Player — mini
- Fixed bar: 40–48px cover thumbnail, title + author (single line, ellipsized), play/pause icon
  button, thin progress line under them.
- **Set apart from the content.** The bar is a libadwaita `card` (rounded, card background, the
  stock card shadow) inset 6px on every side, sitting on a strip styled exactly like the tab bar
  below it (`actionbar`'s header-bar colour with its top shade line). Bar and tab bar read as one
  bottom area, separated from whatever scrolls above by that line, and the card stands on it —
  so list rows never look like they run on into the bar. Named colours only, so dark and
  high-contrast follow. *(Field report: on the phone the old flat `toolbar` bar had the page's
  own background and blended into Home and Library.)*
- Swipe-up gesture or tap opens the full player. **Not yet validated**: the swipe-up gesture's
  hit-zone needs to be confirmed against phosh's own edge-swipe gesture zones on real Librem 5
  hardware before being treated as final, since a bottom-edge swipe risks colliding with the
  shell's own overview/app-switcher gesture. Tap-to-expand is the guaranteed fallback regardless
  of what the swipe gesture ends up being scoped to.
- This bar is pinned above the tab bar (phone) or the sidebar/content split (wide) on **every**
  tab, not just Home — see the "Home" and "Library browse" mockups, both of which show it fixed
  below their scrollable content. It reflects whatever's currently loaded regardless of which
  screen the user navigated to since starting playback.
- **Switching books.** Starting a book stops the previous one at once — its position is saved
  first — and the bar switches to the new book's title and cover immediately, with an empty
  progress line, while the new book resolves (network lookup, then loading its file). Nothing
  acts on a book until it has loaded: seeking does nothing meanwhile, play/pause (from the bar,
  MPRIS, a headphone unplug or a phone call) only decide whether it starts playing once loaded,
  and tapping the bar opens the full player once it has. A tapped chapter is part of the start
  itself. Every start, load, seek, play and pause is logged, as is anything discarded because a
  newer start or load replaced it. *(Field report: the previous book used to stay loaded through
  the whole resolve, and a rewind in that window loaded its audio behind the new book's title.)*
- **Starting the book that's already loaded** (Play, Resume or a chapter on its detail screen)
  doesn't load it again: a chapter is a seek, and otherwise it just plays from where it is.
- **Finished books.** Once a book reaches its end, or is marked finished, it stays finished
  through any later pause, quit, book switch or reconnect. Play on it starts it over from the
  beginning. An end of the stream on the last file that comes more than a minute (but less than
  10% of the file) before its reported length pauses there without marking the book finished;
  Play loads it again at that spot, and ending at the same spot again is the real end.
- **Recovering from a lost stream.** A stream error while playing (a dropped connection, an
  expired token) reloads the file once by itself at the same position; a second one within a
  minute stops with the error banner. A stream that failed while paused, or that has been paused
  for five minutes or more, is loaded afresh on Play rather than resumed. A seek that never lands
  reloads the file once at the target; if that fails too, playback stops with an error and the
  target stays the saved position.
- **Pausing then seeking** (rewind, scrub, speed) keeps the pipeline paused while it re-buffers;
  it is never mistaken for a pause that failed.
- **A speed picked while paused on a stream** is shown at once and applied when playback resumes
  — changing the rate is a network seek nobody can hear. Speeds outside the picker's range (or
  not numbers) are brought into range or ignored.
- **A book whose files are on the device starts from them.** Downloaded tracks are always
  preferred over the stream, online or not. If the track to start in is downloaded, the start
  waits at most ~3 s for the server (which, if it answers, still supplies fresh metadata and
  progress from other devices) — and not at all while offline mode is on. If that track isn't
  downloaded the server is needed and is waited for as before. The log says how many tracks are
  on the device and, for a streamed track that has a download, why it can't be used.
- **Starting a book opens the full player at once**, in a loading state (controls greyed out),
  instead of waiting for the load; the book that is starting counts as the current one, so
  Mark as finished / Reset progress chosen meanwhile are applied once it has loaded.
- **The server's track timeline is the book's timeline.** A file that turns out longer or shorter
  than the server said does not move the later files' offsets (progress is saved against them, and
  other clients read it back the same way); the position shown stalls at a file's end rather than
  running into the next file's part early.
- **Quitting saves the position** however the app is asked to go: Ctrl+Q, closing the window,
  SIGTERM or Ctrl+C. Home and Library sync leave the loaded book's progress to the player.
- **A bouncing headphone jack** doesn't resume playback: a replug resumes only after it has held
  for about 300 ms. An unplug still pauses at once.
- **Replacing the shell** (account switch, sign-out) stops the old player, saves its position and
  takes it off the system media controls.

### System media integration
Lissen's Android build posts a persistent MediaStyle notification — cover art, transport controls,
a seek bar — so playback is visible and controllable from the lock screen and notification shade
even when the app isn't focused. The direct GNOME/phosh equivalent is **not** a hand-drawn
`GNotification`, but exporting the **MPRIS2** D-Bus interfaces
(`org.mpris.MediaPlayer2`/`org.mpris.MediaPlayer2.Player`) from `abs-player`. GNOME Shell and phosh
already know how to render cover art, title/author, transport controls, and a scrub position from
those properties on their own system surfaces — the Shell's quick-settings media widget and the
lock screen's media card — with no custom UI code needed for either. See the
`SystemMediaWidget.dc.html` mockup, which depicts the phosh lock screen card this produces (marked
in the mockup itself as OS-rendered, not app UI, since there's nothing here for this app to paint).
- **Properties to keep current**, on every playback state change (play/pause/seek/position tick),
  not just at session start: `PlaybackStatus`, `Metadata` (`xesam:title`, `xesam:artist`,
  `mpris:artUrl` pointing at the cached cover file, `mpris:length`), `Position`, `Rate`.
- **Methods**: `PlayPause`/`Play`/`Pause`, `Seek`, and `Next`/`Previous` mapped to the full
  player's next/previous chapter buttons rather than a literal track change. Phosh's card draws
  its own seek buttons (`Seek` −10 s / +30 s) between ⏮ and ⏭, so skipping is already covered
  there. A book without chapters maps them to skip-forward/back-N-seconds instead, so ⏮/⏭ never
  go dead.
- A plain `GNotification` is deliberately **not** part of this design: the quick-settings/lock-screen
  media widget already gives always-on visibility and controls without user action, so a second,
  separately-dismissible notification would just duplicate it.

### Hardware controls & interruptions
- **Volume keys**: map directly to system output volume, never repurposed for skip-forward/back
  or other playback actions — this device is a phone, and volume keys doubling as anything else
  would surprise anyone used to normal phone behavior.
- **Call interruption**: MPRIS is outbound-only (it exposes this app's player state and accepts
  transport commands from the shell) and has no concept of inbound "audio focus" events, so it
  can't be used to detect an incoming call — unlike Android, Linux mobile has no OS-level audio
  focus API. Instead, `abs-player` watches call state via **ModemManager**
  (`org.freedesktop.ModemManager1`'s voice-call interface):
  - **Pause as soon as the phone rings** (or, for a call this phone places, as soon as it
    dials) — not only once the call is picked up. ModemManager exports an incoming call already
    in its ringing state, with no state-change signal for the ringing itself, so the watcher
    picks new calls up from the voice interface's `CallAdded` and reads their state; watching
    state changes alone only ever saw the pickup (the first version's Librem 5 field report).
  - **Resume if the call is never answered** — rejected, missed, or the caller hung up first:
    nothing interrupted the listener but the ringing, so the book carries on. Only when the
    call's own pause is still the reason playback is paused: if the user paused or played
    while it rang, or playback was already paused, the call ending changes nothing.
  - **Stay paused after a call that was answered**, and after any outgoing call — the user
    resumes manually via the mini-player or the lock-screen MPRIS card, since auto-resuming
    audio at an arbitrary moment after a conversation (still talking, walking away, etc.) would
    be more surprising than useful.
  Every call state ModemManager reports, and every decision taken on it, is logged at the
  default level, like headphone plug events.
- **Headphone unplug**: MPRIS likewise can't see audio-routing events, and the audio server
  (PulseAudio / PipeWire's `pipewire-pulse` socket) handles rerouting silently — on unplug the
  stream simply moves to the speakers and playback would continue, unheard, with progress
  advancing. So `abs-player` watches the audio server directly (the same libpulse GStreamer's
  `pulsesink` already uses) and pauses when the headphone output goes away: a wired jack
  unplug (kernel jack detection, reported as the headphone port's availability) or a Bluetooth
  device disconnecting (its sink disappearing). Known limit: outputs without jack detection
  (some USB DACs report port availability "unknown") can't be detected — Bluetooth always can.
  Both behaviors are configurable in Settings → Playback:
  - **Pause when headphones disconnect** — on by default; the failure mode it prevents (an
    audiobook blaring from the speaker, position advancing unheard) outweighs the rare
    false pause. Turning it off makes unplugging change nothing.
  - **Resume when headphones reconnect** — off by default (matching VLC's "Resume on headset
    insertion" and AntennaPod's "unpause on reconnection", the only other apps that offer
    this). It **only** lifts a pause caused by the disconnect itself: a manual pause, a phone
    call, a sleep timer or end-of-book are never overridden by a reconnection, and the switch
    is insensitive while pause-on-disconnect is off (a replug has nothing to act on then).
- **Screen-off suspend**: the phone must not suspend mid-book just because the screen locked —
  the app holds a GTK application-level suspend inhibitor (`org.gnome.SessionManager` under
  GNOME/Phosh; the `org.freedesktop.portal.Inhibit` portal under Flatpak, no extra manifest
  permission needed) for exactly as long as something is playing, released the instant it pauses
  or stops. Deliberately `SUSPEND` only, never `IDLE` — the screen still blanks and locks as
  normal, only the SoC is kept awake to keep decoding and streaming audio.

### Player — full
- `AdwNavigationPage` with a transparent/blurred header (down-chevron to collapse, `⋯` menu for
  "Sleep timer", "Playback speed").
- Large cover art, title, author, and the current chapter (`<title> · n of m`, one ellipsized
  line; tapping it opens the chapters list), then a `GtkScale`-based scrubber with
  elapsed/remaining time labels. With chapters, the scrubber and both labels cover the current
  chapter (dragging seeks inside it), and a centred "<h:mm:ss> left in book" label between them
  keeps the book's own time on screen. Without chapters they cover the whole book. The mini bar's
  progress line and MPRIS stay book-level.
- Transport row: previous chapter, skip-back-N-seconds, play/pause (large),
  skip-forward-N-seconds, next chapter. Previous chapter restarts the current chapter when more
  than 3 s into it, and otherwise goes to the one before. Next chapter does nothing in the last
  chapter. A book without chapters hides both chapter buttons.
- Secondary row: playback speed button (cycles/opens popover with 0.8×–3.0× options), sleep-timer
  button (opens popover: off / 15 / 30 / 45 min / end-of-chapter), chapters button (opens a sheet
  listing chapters, current one highlighted).
- This screen carries the app's only arrow-key bindings (skip back/forward), plus the speed,
  chapters, and sleep-timer keys — all defined, with their scoping rationale, in § 6.
- **Cover viewer.** Tapping the cover (here or on Item detail) opens it full-screen in the app, on
  black, with ✕ top left and the title at the top; ✕, Escape and a swipe down (when not zoomed)
  return to the page it was opened from, as it was.
  - **The original, not the cached copy.** The cover cache holds the server's resize (400 px
    wide). The viewer fetches the uploaded original (`/api/items/{id}/cover?raw=1`) and keeps it
    in the cover cache, so opening it again is instant and works offline. It's scaled down to
    2560 px on its long side at most.
  - **Feedback while it loads.** A press dims and shrinks the cover slightly, and the viewer opens
    on release without waiting. It shows, in order, the texture that was on screen, the
    uncropped small copy, then the original with a short crossfade. A pill at the bottom says
    "Downloading full size… N %" and then "Preparing full size…", and disappears once the
    original is shown.
  - **Zoom.** Pinch (around the fingers, 1×–6×), one-finger pan, double-tap toggles fit / 2.5×,
    and Ctrl+scroll on a desktop.
  - **Offline.** Without a cached original it shows the small copy with "Showing a smaller copy —
    the full-size cover needs a connection".
  - **Low memory mode.** The small copy only (about 0.6 MB as a texture, released on close). The
    original is never fetched or decoded, zoom stops at 3×, and the pill says "Low memory mode is
    on — showing a smaller copy".

### Downloads
- `AdwPreferencesPage`-style grouped list: a summary row (storage used / device free space), then
  an `AdwActionRow` per downloaded item with a remove button. Empty state via `AdwStatusPage`.
- An item still mid-download shows a progress indicator in place of the remove button plus a
  **cancel** action (≥44×44px, spaced apart from the row's own tap target per the touch-target
  note above) that aborts that item's download in progress — same resumable-download behavior as
  Item detail: cancelling stops the transfer but keeps whatever chapters already completed.

### Settings
- `AdwPreferencesPage` with groups: **Account**, **Servers**, **Playback** (default speed,
  skip-forward/back intervals, sleep-timer default), **Appearance** (follow system / light /
  dark; language), **About** (version, license, links).
- **Partially built** (this is a living spec; the screen grows group by group): the **Playback**
  group is real — the headphone-behavior switches from "Hardware controls & interruptions"
  (pause-on-disconnect on, resume-on-reconnect off, the latter insensitive while the former is
  off), the default-speed combo (the same preset list as the player's speed popover, applied to
  every new playback), the skip-back/forward interval combos (which the transport buttons, arrow
  keys and MPRIS next/previous read live), and the Wi-Fi-only downloads switch (applied live to
  the download manager). The **Appearance** group (Theme: system/light/dark, applied via
  `AdwStyleManager` immediately and at startup; **Language**: "System (<language>)" by default,
  or an explicit language — stored at once, applied at the next start, see `docs/i18n.md`), the **About** row, and the **Account** and
  **Servers** groups (below) are real too. The one remaining stub: Playback's
  sleep-timer-default row.
- **Low memory mode** (Playback group, next to "Buffer streams in bursts"): one switch for a
  phone short on RAM. It hides covers everywhere (Home, Library, Item detail, Player, mini bar,
  Downloads show their placeholders — nothing is decoded; a tapped cover still opens the small
  copy in the cover viewer), shrinks the cover texture cache from
  24 MiB to 4 MiB, and opens the database with 3 connections and a small page cache instead of 5
  and the default (the database part applies on the next launch; the rest applies at once).
  Burst buffering is **not** changed by it — it stays the user's own choice — but while both are
  on, the burst row adds a line saying it uses up to 64 MB of buffer and suggesting turning it off.
  (Independently of the mode, switching Library between grid and list now empties the container
  that is no longer shown.)
- **Account** group: one row showing the *active* server/account — username as title,
  `host · active` as subtitle — with a chevron; tapping it pushes the active server's
  **Connection** page. The mockup's second row ("Switch or manage servers") is deliberately
  dropped: both groups scroll on one page, so the Servers list directly below *is* that surface,
  and a second row pointing at it would be dead UI (documented deviation). There is still no
  top-level "Sign Out" action — with multiple servers supported, "sign out" is ambiguous about
  *which* account, so it lives per server below.
- **Servers** group: one `AdwActionRow` per configured server (host as title, logged-in username
  and "active" marker as subtitle), each with a trailing menu button (`⋯` / `GtkMenuButton`,
  ≥44×44px and spaced apart from the row body's own tap target per the touch-target note above)
  opening a popover with **Switch to this server** (insensitive for the already-active server),
  **Sign Out**, and **Remove Server** (destructive style) — Sign Out and Remove Server confirmed
  first with a `GtkMessageDialog`, since both destroy local data (progress rows cascade with the
  account; a removed server also purges its on-disk cover/download files, the same cleanup a
  server switch via re-login performs). Every action ends by handing control back to the shell
  rebuilder: rebuilt from the database, or the first-run Welcome screen when no active account
  is left (switching or signing out also ends the session that was playing — the shell, and with
  it the player, is rebuilt). Tapping a server row's body (as opposed to its `⋯` menu) pushes
  that server's **Connection** page. An "Add Server" row closes the group: it opens the same
  Welcome flow the first run uses (previously only reachable when no account was active — this
  row is what makes a second server possible), with a Cancel affordance back to the shell; a
  successful connect activates the new account and rebuilds the shell.
- **About** group: a single `AdwActionRow` with the app version as its subtitle, opening an
  in-window About screen on tap (app name, version, a Website row, a License row) — swapping the
  main window's own content via `push_about`, with its own back button, the same way Settings'
  own Connection page (`push_connection`) and every other secondary screen in this app already
  work, rather than opening a second top-level window. It used to be `adw::AboutWindow` (chosen
  because `AdwAboutDialog` needs libadwaita 1.5+, out of reach of this app's `v1_2` feature
  ceiling — see `app/Cargo.toml`'s `adw` dependency) — a real Librem 5 field report found that
  genuine second window had no reliable way to be dismissed under Phosh's default compositor
  (phoc), so About stopped being the one screen in this app that opens a second window. The
  version is sourced at compile time from `CARGO_PKG_VERSION` — the workspace's single version
  number — and is the exact same string the `--version`/`-v` command-line flags print, so it can
  never drift between the two.

### Connection
- Per-server connection settings, pushed from a server row in Settings' Servers group (or the
  Account row, for the active server). **Partially built**: the navigation itself can't be an
  `AdwNavigationPage` — `AdwNavigationView` is v1.4+, out of this crate's `v1_2` ceiling — so
  the page is shown by swapping the window's content (the same mechanism the full player uses),
  with a back button in its header that restores the captured shell widget; one instance per
  configured server.
- **Server connection** group (real): a single `AdwActionRow` showing the server's full URL as
  its subtitle (monospace, via a small per-process stylesheet targeting the row's internal
  subtitle label, since it's a literal value being confirmed rather than a label) with a
  trailing info-button opening a popover explaining what this connection is used for (every
  server call — syncing, streaming, downloads — goes through it).
- **Advanced** group (real): **Custom Headers** (a save-time-validated `Name: Value` list,
  shown as a count), **Disable SSL verification** (a switch; self-hosted servers behind
  self-signed certificates), **Client certificate** (a PKCS#12 bundle + export password picked
  from disk and validated at save time with the exact read+parse a connection mint performs —
  PKCS#12 rather than combined PEM because this workspace's TLS backend is native-tls, whose
  PEM identity loader rejects the widespread RSA PKCS#1 key encoding), **Local network server
  address** (used instead of the public URL whenever a short reachability probe — cached for a
  minute per session — finds it listening, so home-Wi-Fi traffic stays local and everything
  else falls back cleanly), and **Change User Agent** (empty restores the default,
  `Audiobooklet/<version> (Linux)`, sent on API calls and audio streams alike; it deliberately avoids
  the Audiobookshelf name, per the project's rules for third-party clients). Each row
  persists to the server's table row the moment it's saved and is picked up by the next
  connection mint — sync, downloads, playback — without any rebuild; the editors show a
  value-as-subtitle (or "None"/"Default") where the mockup showed a static description, and
  each saves through the same validation the actual connection uses, so what the dialog
  accepts cannot then fail the connection. Two transport scopes are documented honestly: the
  client certificate applies to reqwest traffic only (GStreamer's souphttpsrc handles mTLS
  through a separate mechanism), and `authorization` is rejected as a custom header — it would
  clobber the app's own login token.
- A destructive **"Disconnect from the Server"** plain-text action (real; styled like
  `AdwButton`'s `destructive-action` but flat, not a filled button, since it's an infrequent,
  page-level action rather than a primary one) sits below the groups, vertically separated
  rather than boxed in its own card. It signs out the server's account — its local progress
  cascades away, the server and its cache survive — after a confirming
  `GtkMessageDialog`, then hands control back to the shell rebuilder (Welcome when no active
  account is left).

## 4. Adaptive/responsive behavior summary

- Breakpoint at 600px (`AdwBreakpoint`), matching libadwaita convention:
  - **< 600px (phone/phosh):** single-pane navigation, bottom tab bar, full-screen player.
  - **≥ 600px (tablet/desktop):** `AdwNavigationSplitView` sidebar + content, player as a
    window-sized dialog rather than full-screen takeover.
  - This is evaluated purely on window width, with no separate orientation-based logic: a Librem 5
    rotated to landscape gets exactly the same sidebar layout as any other window whose width
    crosses 600px, and a portrait window narrower than that stays single-pane regardless of the
    device's physical orientation.
- All list/grid views reflow item counts per row based on available width (`GtkGridView` with a
  minimum tile width, not a fixed column count) so the same layout scales from phone to desktop.
- Touch targets sized per GNOME HIG (minimum 44×44px) throughout, since phosh is a touch shell.
- **Every full-window screen scrolls vertically.** All screens share one window, and a GTK window
  grows to the largest minimum height any screen shown in it ever had — then never shrinks back.
  A screen that can't get shorter than the phone (~648 logical px under phosh on a Librem 5)
  therefore leaves every later screen taller than the display, e.g. Home's tab bar under phosh's
  bottom bar. *(Field report: the full player, unscrolled, measured 631px with a one-line title and
  800px with a long title plus its error banner; after opening it, Home stayed too tall.)* A
  scenario (`main_window_every_screen_fits_a_phone`) holds every screen to ≤ 600px at 360px wide.
- **Icons come from current Adwaita or the app itself.** Every symbolic icon name must be in
  `app/data/stock-icons.txt` (Adwaita's icons, outside its legacy folder, in both bookworm's 43
  and the newest version; `scripts/update-stock-icons.sh` regenerates it), bundled in
  `app/data/icons/`, or libadwaita's own `adw-…`. A legacy or dropped name is drawn from whatever
  the system has instead: a "missing icon" placeholder (the AppImage's filter funnel) or a
  full-colour icon (the flatpak's Library tab). The bundled ones: the Library's books, the Settings
  gear and the filter funnel. `icons_every_icon_the_app_uses_is_in_the_theme` checks this.

## 5. State handling

- **Loading:** `AdwSpinner`/`GtkSpinner` in place of content, skeleton-less (per HIG, prefer
  spinners over skeleton screens for this style of app).
- **Empty:** `AdwStatusPage` with an icon, title, and short description (e.g. "No downloads yet").
- **Error/offline:** a banner at the top of the affected page (the app's own `ErrorBanner`;
  `AdwBanner` needs libadwaita 1.3), plus `AdwToast` for transient errors (e.g. "Failed to sync
  progress — will retry"). The banner is a card: 12px from the screen's edges, 12px padding,
  rounded corners, an amber tint (a server being down isn't the user's fault), a ⚠ glyph, a
  short **bold headline** saying what happened, a dimmed line saying what still works or what
  to do, and a bottom row with "Details" (the raw error, selectable, monospace) and the action
  (Retry / Log in again). Toasts stay at most 20 characters ("Can't reach server", "Connection
  lost") so they're never cut off next to their "View" button; the full sentence is in the
  banner. On an authorization failure (the server rejects
  the session), the banner reads "Signed out by the server" and its button is "Log in again",
  routing to the re-login flow (see Home) — on every screen that shows the banner, not just
  Home. The player's banner says "Can't reach your server" / "This book can't start until it's
  back…" when a book couldn't start, and "Connection lost" when a stream that was playing
  dropped; a book whose length isn't known yet shows no "0:00 / -0:00" times.
- **Listening position:** never lost silently. Progress the server hasn't confirmed (listened to
  offline, or whose push failed) is kept marked on the device and pushed on the next sync or
  reconnect. Signing out or removing a server says so when it would delete such progress.
  Resuming after a pause of a minute or more first checks the server; if another device moved
  the book on, playback continues there and a toast ("Continued at 1:02:03 from another
  device") offers Undo. "Mark as finished" and "Reset progress" toasts offer Undo too. Quitting
  saves the final position, and a seek while paused is saved once seeking stops.
- **Offline-first playback:** downloaded items remain playable and browsable without a server
  connection; the app clearly marks which items are available offline (small download badge on
  covers/rows, and per-chapter offline markers in Item detail). The Home/Library offline-mode
  toggle (see those sections) lets a user deliberately narrow either screen to only such items
  even while online, e.g. before a trip. Starting playback offline works from the locally
  cached track metadata: a fully-downloaded book plays end-to-end with no server contact, a
  partially-downloaded one plays from the resume position and stops cleanly at the first
  missing chapter, and a book never synced on this device (no cached metadata) can't start.
- **Offline mode means no network at all.** While it is on nothing reaches the server: no library
  sync, progress reconcile or cover fetch (Home and Library show what's stored; "Sync now" says
  offline mode is on), no progress push (positions are kept on the device), no token refresh, no
  chapter fetch in Item detail, no download ("turn it off to download"), and no streaming — a part
  of a book that isn't downloaded stops with "This part of the book isn't downloaded — turn off
  offline mode to stream it." Switching it off pushes the kept progress. A file already streaming
  when it is switched on plays to its end (its connection is already open). The login and
  add-server screens aren't affected.

## 6. Keyboard shortcuts

This is a phone-first app, but it runs on desktop GNOME and on phones with attached keyboards
(Librem 5, PinePhone + keyboard case, Bluetooth), so basic keyboard support is part of the
interface, not an afterthought. The bindings below follow GNOME conventions wherever one exists —
`Space` for play/pause and arrow-key seeking match GNOME's own media players (Decibels' shortcuts
dialog is the closest analogue; Totem agrees) — and each action maps 1:1 onto a control that
already exists on-screen, so a keyboard user is never offered anything the touch UI can't do.

| Keys | Action | Scope |
|---|---|---|
| `Space` | Play / pause | Global (no-op when nothing is loaded) |
| `Alt+1` … `Alt+4` | Switch tab: Home / Library / Downloads / Settings | Global |
| `Ctrl+F` | Focus the Library search field (switching to the Library tab first if needed) | Global |
| `Esc` | Collapse the full player back to the shell | Global |
| `Ctrl+?` | Open the keyboard-shortcuts overlay (`GtkShortcutsWindow`) | Global |
| `Ctrl+,` | Open the Settings tab | Global |
| `Ctrl+Q` | Quit | Global |
| `←` / `→` | Skip back / forward by the configured interval (Settings → Playback) | Full player |
| `Ctrl+=` / `Ctrl+−` (also bare `+` / `−`) | Playback speed up / down through the popover's presets | Full player |
| `Ctrl+0` / `0` | Reset playback speed to 1× | Full player |
| `c` | Open the chapters list | Full player |
| `t` | Open the sleep-timer popover | Full player |

Rules, each of which constrains future changes to this table:

- **Typing always wins.** While any text entry has focus (Library search, the Welcome form, the
  Connection page's header editor), single-key bindings like `Space` and `c` must not fire —
  they must type. GTK4 does **not** give this for free: application accelerators
  (`set_accels_for_action`) run in the *capture* phase at the window, ahead of the focused
  widget, so a bare-key accelerator swallows the keypress before a `GtkText` sees it (this is
  how a bare key once stopped typing in the Library search). Hence every modifier-less binding is
  installed by `add_single_key_shortcuts` as a `GtkShortcutController` on the window with
  global scope and the *bubble* phase, which only runs once the focused widget has declined the
  key. Any new modifier-less binding goes there, never through `set_accels_for_action`;
  accelerators with a modifier (`Ctrl+F`, `Alt+1`, …) stay as app accelerators. One deliberate
  consequence: a focused button handles `Space` itself (activating) rather than toggling
  playback, which is standard GTK behaviour.
- **Arrows are deliberately not global.** Every other screen is grids and lists where arrow keys
  are keyboard focus navigation (an accessibility surface) — globalizing skip would redefine
  them out from under that. Hence skip is scoped to the full player, the one screen with nothing
  to focus-navigate, matching Decibels/Totem. The interplay with the scrubber is benign either
  way: if the `GtkScale` has focus, arrows move the slider (i.e. they seek); if nothing is
  focused, the accelerator fires (i.e. it seeks too).
- **No `XF86` media-key bindings.** Hardware play/skip/volume keys arrive through the shell via
  the app's own MPRIS interface (see System media integration); binding them in-app as well
  would double-handle each press. Volume is system-owned outright (see Hardware controls &
  interruptions).
- **Mechanics:** registered as named actions with `set_accels_for_action` (application-scope for
  the global set; window-scope for the full-player set so they simply don't exist elsewhere) and
  listed in a `GtkShortcutsWindow` opened by `Ctrl+?` — the standard GNOME discoverability pair.
  These are the app's first `GAction`s: the existing "no `GMenu`/`GAction`" convention on the
  player screen is about menu *buttons* (plain `GtkMenuButton`/`GtkButton` there), not about
  accelerators, which in GTK only target actions — so this is an idiom extension, not a
  contradiction.
