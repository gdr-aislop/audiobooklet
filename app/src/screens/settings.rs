//! The Settings destination — per `docs/design/ui-spec.md`'s "Settings" section, an
//! `AdwPreferencesPage` under the tab bar. The **Account** group (the active server/account;
//! tapping it pushes the active server's Connection page) and the **Servers** group (one row
//! per configured server with a Switch/Sign Out/Remove menu, plus Add Server — which reuses
//! the Welcome flow, the only way a second server can ever enter the database) are real, as
//! are the **Playback** group (headphone switches, default speed, skip intervals, Wi-Fi-only
//! downloads), the **Appearance** group (Theme, Language) and the **About** row. Still to come:
//! Playback's sleep-timer-default row — it arrives with the sleep-timer popover feature, since
//! every row here is live wiring rather than decoration.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_core::playback::SPEED_PRESETS;
use abs_core::settings::{PlaybackSettings, Theme};
use abs_storage::models::{Account, Server};

use crate::widgets::combo_row;
use abs_storage::AppPaths;

use crate::downloads::DownloadManager;
use crate::i18n::{ntr, tr, tr_args, tr_noop};
use crate::player::PlayerController;

/// Skip-interval choices (seconds) for Settings → Playback — the full player's transport buttons,
/// arrow keys and MPRIS next/previous all step by one of these.
const SKIP_CHOICES: [i64; 7] = [5, 10, 15, 20, 30, 45, 60];

pub struct SettingsScreen {
    pub root: gtk4::Widget,
    #[cfg(test)]
    pub hooks: SettingsHooks,
}

#[cfg(test)]
pub struct SettingsHooks {
    pub pause_on_unplug_switch: gtk4::Switch,
    pub resume_on_replug_switch: gtk4::Switch,
    pub default_speed_row: adw::ComboRow,
    pub skip_back_row: adw::ComboRow,
    pub skip_forward_row: adw::ComboRow,
    pub wifi_only_switch: gtk4::Switch,
    pub wifi_only_row: adw::ActionRow,
    pub burst_buffering_switch: gtk4::Switch,
    pub burst_buffering_row: adw::ActionRow,
    pub low_memory_switch: gtk4::Switch,
    pub anonymize_logs_switch: gtk4::Switch,
    pub open_log_row: adw::ActionRow,
    pub theme_row: adw::ComboRow,
    pub language_row: adw::ComboRow,
    pub about_row: adw::ActionRow,
    pub account_row: adw::ActionRow,
    pub server_rows: Vec<ServerRowHooks>,
    pub add_server_row: adw::ActionRow,
}

#[cfg(test)]
pub struct ServerRowHooks {
    pub row: adw::ActionRow,
    pub switch_item: gtk4::Button,
    pub sign_out_item: gtk4::Button,
    pub remove_item: gtk4::Button,
}

/// A settings page is wired to everything it can edit — the argument count reflects that
/// surface, not a missing params-struct refactor; the allow documents that judgment call.
#[allow(clippy::too_many_arguments)]
pub fn build(
    pool: SqlitePool,
    controller: PlayerController,
    download_manager: DownloadManager,
    playback_settings: PlaybackSettings,
    low_memory_mode: crate::low_memory_mode::LowMemoryModeState,
    theme: Theme,
    paths: AppPaths,
    servers_with_accounts: Vec<(Server, Vec<Account>)>,
    window: adw::ApplicationWindow,
) -> SettingsScreen {
    // The one shared copy of the settings this page edits: each row mutates its own field, then
    // the *whole* struct is persisted (the kv store round-trips everything, so partial writes
    // would silently reset the other fields to their pre-edit values). The theme is stored under
    // its own single key (`save_theme`) and deliberately kept out of this struct — it has no
    // whole-struct overwrite hazard.
    let settings = Rc::new(RefCell::new(playback_settings));
    // Save serialization state — see `persist` for why two saves must never run concurrently.
    let pending_save: Rc<RefCell<Option<PlaybackSettings>>> = Rc::new(RefCell::new(None));
    let writer_running: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Declared here (not at the end, alongside `content`) so every row/action below can capture
    // it — background-write failures (switch account, sign out, remove server, theme, playback
    // settings) need somewhere to report to, and this screen previously had no toast surface at
    // all.
    let toast_overlay = adw::ToastOverlay::new();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&tr("Settings"), "")));

    let page = adw::PreferencesPage::new();

    // --- Account: the one session the shell was built for. The DB enforces at most one active
    // account across all servers (`accounts_one_active_idx`), and the signed-in shell exists by
    // that grace — so this row is a fact, not a picker; switching happens per server below.
    // (The mockup's "Switch or manage servers" row is deliberately absent: both groups scroll on
    // one page, so the Servers list *is* that surface — no second row can add anything.) ---
    // The post-mutation shell handoff every session-changing action in these groups ends with —
    // rebuilt from the database, or the Welcome screen when no active account is left.
    let on_session_changed: Rc<dyn Fn()> = {
        let pool = pool.clone();
        let paths = paths.clone();
        let window = window.clone();
        std::rc::Rc::new(move || {
            crate::application::show_main_or_welcome(&window, pool.clone(), paths.clone(), playback_settings)
        })
    };

    let (active_server, active_account) = servers_with_accounts
        .iter()
        .find_map(|(server, accounts)| {
            accounts.iter().find(|account| account.is_active).map(|account| (server, account))
        })
        .expect("the signed-in shell requires an active account");

    let account_group = adw::PreferencesGroup::new();
    account_group.set_title(&tr("Account"));
    let account_row = adw::ActionRow::builder()
        .title(&active_account.username)
        // TRANSLATORS: Subtitle of the signed-in account's row. {host} is the server's address; "active" marks it as the server in use.
        .subtitle(tr_args("{host} · active", &[("host", host_of(&active_server.url))]))
        .build();
    account_row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    account_row.set_activatable(true);
    account_row.connect_activated({
        let pool = pool.clone();
        let window = window.clone();
        let server = active_server.clone();
        let account = active_account.clone();
        let on_session_changed = on_session_changed.clone();
        move |_| push_connection(&pool, &server, Some(account.clone()), &window, &on_session_changed)
    });
    account_group.add(&account_row);
    page.add(&account_group);

    // --- Servers: one row per configured server — host as title, its logged-in username (with
    // an "active" marker on the active server's row) as subtitle — each with a trailing ⋯ menu.
    // The menu button is its own ≥44px hit target, spaced from the row body's tap target (the
    // spec's touch-target note). This is where sign-out actually lives — scoped to one
    // server/account at a time, since a global "Sign Out" would be ambiguous with multiple
    // servers supported. ---
    let servers_group = adw::PreferencesGroup::new();
    servers_group.set_title(&tr("Servers"));

    #[cfg(test)]
    let mut server_rows_hooks = Vec::new();
    for (server, accounts) in &servers_with_accounts {
        let is_active_server = accounts.iter().any(|account| account.is_active);
        // The account the row speaks for: the active one if it's here, else the first.
        let shown_account = accounts.iter().find(|account| account.is_active).or(accounts.first());
        let subtitle = match shown_account {
            // TRANSLATORS: Subtitle of a server's row. {username} is the account signed in on that server; "active" marks the server in use.
            Some(account) if is_active_server => tr_args("{username} · active", &[("username", account.username.as_str())]),
            Some(account) => account.username.clone(),
            None => tr("No signed-in account"),
        };
        let row = adw::ActionRow::builder().title(host_of(&server.url)).subtitle(&subtitle).build();

        let menu_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();

        // "Switch to this server" flips the DB's active account to this server's and rebuilds
        // the shell around it — pointless when this server's account is already the active one,
        // and impossible when the server has none.
        let switch_item = gtk4::Button::builder()
            .label(tr("Switch to this server"))
            .css_classes(["flat"])
            .height_request(44)
            .build();
        switch_item.set_sensitive(!is_active_server && !accounts.is_empty());
        menu_box.append(&switch_item);

        let sign_out_item = gtk4::Button::builder()
            .label(tr("Sign Out"))
            .css_classes(["flat"])
            .height_request(44)
            .build();
        sign_out_item.set_sensitive(!accounts.is_empty());
        menu_box.append(&sign_out_item);

        // Not `flat`: with `destructive-action` that draws the label in the popover's own
        // background colour (white on white with libadwaita 1.5, found in the 2026-10-12 pass) —
        // the same reason `widgets/item_options_menu.rs` gives.
        let remove_item = gtk4::Button::builder()
            .label(tr("Remove Server"))
            .css_classes(["destructive-action"])
            .margin_top(6)
            .height_request(44)
            .build();
        menu_box.append(&remove_item);

        let menu_button = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(tr("Server options"))
            .css_classes(["flat"])
            .width_request(44)
            .height_request(44)
            .valign(gtk4::Align::Center)
            .build();
        crate::a11y::name(&menu_button, &tr("Server options"));
        menu_button.set_popover(Some(&gtk4::Popover::builder().child(&menu_box).build()));
        row.add_suffix(&menu_button);

        // Tapping the row's body (not the ⋯ menu) pushes the server's Connection page.
        row.set_activatable(true);
        row.connect_activated({
            let pool = pool.clone();
            let window = window.clone();
            let server = server.clone();
            let account = shown_account.cloned();
            let on_session_changed = on_session_changed.clone();
            move |_| push_connection(&pool, &server, account.clone(), &window, &on_session_changed)
        });

        servers_group.add(&row);

        if let Some(account) = shown_account {
            switch_item.connect_clicked({
                let pool = pool.clone();
                let paths = paths.clone();
                let window = window.clone();
                let account_id = account.id.clone();
                let toast_overlay = toast_overlay.clone();
                move |button| {
                    let pool = pool.clone();
                    let paths = paths.clone();
                    let window = window.clone();
                    let account_id = account_id.clone();
                    let toast_overlay = toast_overlay.clone();
                    // A second click while the shell is rebuilding would build another one.
                    button.set_sensitive(false);
                    let button = button.clone();
                    glib::spawn_future_local(async move {
                        if let Err(err) = abs_core::accounts::switch_active_account(&pool, &account_id).await {
                            button.set_sensitive(true);
                            crate::error_reporting::report_background_error(&toast_overlay, &tr("Switching accounts"), err);
                            return;
                        }
                        crate::application::show_main_or_welcome(&window, pool, paths, playback_settings);
                    });
                }
            });

            // Sign-out removes the account row — its local progress cascades away
            // with it (per-account data, keyed by FK). The server and its cached
            // libraries/items/downloads survive. Destructive enough to confirm first.
            sign_out_item.connect_clicked({
                let pool = pool.clone();
                let paths = paths.clone();
                let window = window.clone();
                let account_id = account.id.clone();
                let username = account.username.clone();
                let was_active = account.is_active;
                let toast_overlay = toast_overlay.clone();
                move |_| {
                    let dialog_window = window.clone();
                    let pool = pool.clone();
                    let paths = paths.clone();
                    let window = window.clone();
                    let account_id = account_id.clone();
                    let toast_overlay = toast_overlay.clone();
                    let username = username.clone();
                    glib::spawn_future_local(async move {
                        let unpushed = abs_storage::repo::progress::count_needing_push(&pool, Some(&account_id), None).await.unwrap_or(0);
                        confirm(
                            &dialog_window,
                            // TRANSLATORS: Dialog heading. {username} is the account being signed out.
                            &tr_args("Sign out of {username}?", &[("username", username.as_str())]),
                            &format!(
                                "{}{}",
                                // TRANSLATORS: Dialog body. {username} is the account being signed out.
                                tr_args("{username}'s listening progress stored on this device will be removed — everything on the server stays where it is.", &[("username", username.as_str())]),
                                unpushed_progress_warning(unpushed)
                            ),
                            &tr("Sign Out"),
                            Rc::new(move || {
                                let pool = pool.clone();
                                let paths = paths.clone();
                                let window = window.clone();
                                let account_id = account_id.clone();
                                let toast_overlay = toast_overlay.clone();
                                glib::spawn_future_local(async move {
                                    if let Err(err) = abs_core::accounts::sign_out(&pool, &account_id, crate::password_keyring::store().as_ref()).await {
                                        crate::error_reporting::report_background_error(&toast_overlay, &tr("Signing out"), err);
                                        return;
                                    }
                                    // A server that isn't the active one: the rebuilt shell opens on Settings, where the
                                    // user was, instead of Home.
                                    crate::application::show_main_or_welcome_on(&window, pool, paths, playback_settings, (!was_active).then_some("settings"));
                                });
                            }),
                        );
                    });
                }
            });
        }

        // Removing the server cascades every account/library/item/download record for it
        // (abs_core::accounts::remove_server) and — unlike sign-out — also purges the server's
        // on-disk cover/download files, the same cleanup a server switch via relogin performs.
        remove_item.connect_clicked({
            let pool = pool.clone();
            let paths = paths.clone();
            let download_manager = download_manager.clone();
            let window = window.clone();
            let server_id = server.id.clone();
            let server_host = host_of(&server.url).to_string();
            let username = shown_account.map(|account| account.username.clone());
            let was_active = is_active_server;
            let toast_overlay = toast_overlay.clone();
            move |_| {
                let dialog_window = window.clone();
                let pool = pool.clone();
                let paths = paths.clone();
                let download_manager = download_manager.clone();
                let window = window.clone();
                let server_id = server_id.clone();
                let toast_overlay = toast_overlay.clone();
                let server_host = server_host.clone();
                let username = username.clone();
                glib::spawn_future_local(async move {
                    let unpushed = abs_storage::repo::progress::count_needing_push(&pool, None, Some(&server_id)).await.unwrap_or(0);
                    let heading = match &username {
                        // TRANSLATORS: Dialog heading. {server} is the address of the server being removed, {username} the account signed in on it (one server can be listed once per account).
                        Some(username) => tr_args("Remove {server} ({username})?", &[("server", server_host.as_str()), ("username", username.as_str())]),
                        // TRANSLATORS: Dialog heading. {server} is the address of the server being removed.
                        None => tr_args("Remove {server}?", &[("server", server_host.as_str())]),
                    };
                    confirm(
                        &dialog_window,
                        &heading,
                        &format!(
                            "{}{}",
                            tr("Every account, cached library, item and downloaded file for this server will be removed from this device. Everything on the server itself stays untouched."),
                            unpushed_progress_warning(unpushed)
                        ),
                        &tr("Remove Server"),
                        Rc::new(move || {
                            let pool = pool.clone();
                            let paths = paths.clone();
                            let window = window.clone();
                            let server_id = server_id.clone();
                            let toast_overlay = toast_overlay.clone();
                            // Its downloads still running would keep writing into the files
                            // removed below.
                            download_manager.cancel_server(&server_id);
                            glib::spawn_future_local(async move {
                                if let Err(err) = abs_core::accounts::remove_server(&pool, &server_id, crate::password_keyring::store().as_ref()).await {
                                    crate::error_reporting::report_background_error(&toast_overlay, &tr("Removing the server"), err);
                                    return;
                                }
                                if let Err(err) = paths.purge_server_data(&server_id).await {
                                    tracing::warn!(%err, server_id = %server_id, "couldn't purge the removed server's on-disk files; they are orphaned but harmless");
                                }
                                // A server that isn't the active one: the rebuilt shell opens on Settings, where the
                                // user was, instead of Home.
                                crate::application::show_main_or_welcome_on(&window, pool, paths, playback_settings, (!was_active).then_some("settings"));
                            });
                        }),
                    );
                });
            }
        });

        #[cfg(test)]
        server_rows_hooks.push(ServerRowHooks {
            row: row.clone(),
            switch_item,
            sign_out_item,
            remove_item,
        });
    }

    // "Add Server" — the only way a second server can ever enter the database: the Welcome flow
    // it reuses is otherwise only reachable when no account is active at all.
    let add_server_row = adw::ActionRow::builder().title(tr("Add Server")).build();
    add_server_row.add_prefix(&gtk4::Image::from_icon_name("list-add-symbolic"));
    add_server_row.set_activatable(true);
    add_server_row.connect_activated({
        let pool = pool.clone();
        let paths = paths.clone();
        let window = window.clone();
        move |_| crate::application::show_add_server(&window, pool.clone(), paths.clone(), playback_settings)
    });
    servers_group.add(&add_server_row);
    page.add(&servers_group);

    let playback_group = adw::PreferencesGroup::new();
    playback_group.set_title(&tr("Playback"));

    let pause_on_unplug_switch = gtk4::Switch::new();
    pause_on_unplug_switch.set_valign(gtk4::Align::Center);
    // Both properties: `state` is what user toggles drive (via `state-set`), `active` is what
    // the next `activate` toggles *from* — initializing only one of them would make the first
    // activation jump to a stale value.
    pause_on_unplug_switch.set_state(playback_settings.pause_on_headphone_unplug);
    pause_on_unplug_switch.set_active(playback_settings.pause_on_headphone_unplug);
    let pause_row = adw::ActionRow::builder()
        .title(tr("Pause when headphones disconnect"))
        .subtitle(tr("Wired headphones unplugged, or Bluetooth audio lost"))
        .build();
    pause_row.add_suffix(&pause_on_unplug_switch);
    pause_row.set_activatable_widget(Some(&pause_on_unplug_switch));
    playback_group.add(&pause_row);

    let resume_on_replug_switch = gtk4::Switch::new();
    resume_on_replug_switch.set_valign(gtk4::Align::Center);
    resume_on_replug_switch.set_state(playback_settings.resume_on_headphone_replug);
    resume_on_replug_switch.set_active(playback_settings.resume_on_headphone_replug);
    resume_on_replug_switch.set_sensitive(playback_settings.pause_on_headphone_unplug);
    let resume_row = adw::ActionRow::builder()
        .title(tr("Resume when headphones reconnect"))
        .subtitle(tr("Only undoes a pause caused by disconnecting — never a manual pause or a call"))
        .build();
    resume_row.add_suffix(&resume_on_replug_switch);
    resume_row.set_activatable_widget(Some(&resume_on_replug_switch));
    playback_group.add(&resume_row);

    // A replug can only ever lift an unplug pause, so with pause-on-unplug off, resume has
    // nothing to act on — reflecting that in the UI beats letting it silently do nothing.
    pause_on_unplug_switch.connect_state_set({
        let resume_on_replug_switch = resume_on_replug_switch.clone();
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        let writer_running = writer_running.clone();
        let controller = controller.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, state| {
            settings.borrow_mut().pause_on_headphone_unplug = state;
            persist(&pool, &pending_save, &writer_running, &settings, &controller, &download_manager, &toast_overlay);
            resume_on_replug_switch.set_sensitive(state);
            glib::signal::Propagation::Proceed
        }
    });
    resume_on_replug_switch.connect_state_set({
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        let writer_running = writer_running.clone();
        let controller = controller.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, state| {
            settings.borrow_mut().resume_on_headphone_replug = state;
            persist(&pool, &pending_save, &writer_running, &settings, &controller, &download_manager, &toast_overlay);
            glib::signal::Propagation::Proceed
        }
    });

    // Start-of-session speed and skip intervals — the same presets/choices the full player's
    // speed popover and transport buttons use (one shared constant each, so they can't drift).
    // `set_selected` fires the same `notify::selected` handler user choice does, so the initial
    // value goes through the identical path; the handlers are connected after that.
    let default_speed_row = combo_row(&tr("Default speed"), &tr("Speed every book starts at"), &speed_labels());
    default_speed_row.set_selected(speed_index(playback_settings.default_speed));
    playback_group.add(&default_speed_row);
    let skip_back_row = combo_row(&tr("Skip back"), &tr("Seconds the back button and ← key jump"), &skip_labels());
    skip_back_row.set_selected(skip_index(playback_settings.skip_back_seconds, 15));
    playback_group.add(&skip_back_row);
    let skip_forward_row = combo_row(&tr("Skip forward"), &tr("Seconds the forward button and → key jump"), &skip_labels());
    skip_forward_row.set_selected(skip_index(playback_settings.skip_forward_seconds, 30));
    playback_group.add(&skip_forward_row);

    let wifi_only_switch = gtk4::Switch::new();
    wifi_only_switch.set_valign(gtk4::Align::Center);
    wifi_only_switch.set_state(playback_settings.wifi_only_downloads);
    wifi_only_switch.set_active(playback_settings.wifi_only_downloads);
    let wifi_only_row = adw::ActionRow::builder()
        .title(tr("Wi-Fi only downloads"))
        .subtitle(wifi_only_subtitle(download_manager.can_detect_metered()))
        .build();
    wifi_only_row.add_suffix(&wifi_only_switch);
    wifi_only_row.set_activatable_widget(Some(&wifi_only_switch));
    playback_group.add(&wifi_only_row);

    let burst_buffering_switch = gtk4::Switch::new();
    burst_buffering_switch.set_valign(gtk4::Align::Center);
    burst_buffering_switch.set_state(playback_settings.burst_buffering);
    burst_buffering_switch.set_active(playback_settings.burst_buffering);
    let burst_buffering_row = adw::ActionRow::builder().title(tr("Buffer streams in bursts")).subtitle(tr(BURST_BUFFERING_SUBTITLE)).build();
    burst_buffering_row.add_suffix(&burst_buffering_switch);
    burst_buffering_row.set_activatable_widget(Some(&burst_buffering_switch));
    playback_group.add(&burst_buffering_row);

    // "Low memory mode" lives next to it: the two interact (burst buffering buffers up to 64 MB),
    // and the burst row says so while both are on. Burst buffering stays the user's own choice.
    let low_memory_switch = gtk4::Switch::new();
    low_memory_switch.set_valign(gtk4::Align::Center);
    low_memory_switch.set_state(low_memory_mode.get());
    low_memory_switch.set_active(low_memory_mode.get());
    let low_memory_row = adw::ActionRow::builder()
        .title(tr("Low memory mode"))
        .subtitle(tr("Hides covers and keeps less in memory. The database part applies on the next launch."))
        .build();
    low_memory_row.add_suffix(&low_memory_switch);
    low_memory_row.set_activatable_widget(Some(&low_memory_switch));
    playback_group.add(&low_memory_row);
    let refresh_burst_hint = {
        let burst_buffering_row = burst_buffering_row.clone();
        move |burst_buffering: bool, low_memory: bool| burst_buffering_row.set_subtitle(&burst_buffering_subtitle(burst_buffering, low_memory))
    };
    refresh_burst_hint(burst_buffering_switch.is_active(), low_memory_switch.is_active());

    default_speed_row.connect_selected_notify({
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        let writer_running = writer_running.clone();
        let controller = controller.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let toast_overlay = toast_overlay.clone();
        move |row| {
            settings.borrow_mut().default_speed = SPEED_PRESETS[row.selected() as usize];
            persist(&pool, &pending_save, &writer_running, &settings, &controller, &download_manager, &toast_overlay);
        }
    });
    skip_back_row.connect_selected_notify({
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        let writer_running = writer_running.clone();
        let controller = controller.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let toast_overlay = toast_overlay.clone();
        move |row| {
            settings.borrow_mut().skip_back_seconds = SKIP_CHOICES[row.selected() as usize];
            persist(&pool, &pending_save, &writer_running, &settings, &controller, &download_manager, &toast_overlay);
        }
    });
    skip_forward_row.connect_selected_notify({
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        let writer_running = writer_running.clone();
        let controller = controller.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let toast_overlay = toast_overlay.clone();
        move |row| {
            settings.borrow_mut().skip_forward_seconds = SKIP_CHOICES[row.selected() as usize];
            persist(&pool, &pending_save, &writer_running, &settings, &controller, &download_manager, &toast_overlay);
        }
    });
    wifi_only_switch.connect_state_set({
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        let writer_running = writer_running.clone();
        let controller = controller.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, state| {
            settings.borrow_mut().wifi_only_downloads = state;
            persist(&pool, &pending_save, &writer_running, &settings, &controller, &download_manager, &toast_overlay);
            glib::signal::Propagation::Proceed
        }
    });
    burst_buffering_switch.connect_state_set({
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        let writer_running = writer_running.clone();
        let controller = controller.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let toast_overlay = toast_overlay.clone();
        let low_memory_switch = low_memory_switch.clone();
        let refresh_burst_hint = refresh_burst_hint.clone();
        move |_, state| {
            refresh_burst_hint(state, low_memory_switch.is_active());
            settings.borrow_mut().burst_buffering = state;
            persist(&pool, &pending_save, &writer_running, &settings, &controller, &download_manager, &toast_overlay);
            glib::signal::Propagation::Proceed
        }
    });

    let low_memory_handler = low_memory_switch.connect_state_set({
        let low_memory_mode = low_memory_mode.clone();
        let burst_buffering_switch = burst_buffering_switch.clone();
        let refresh_burst_hint = refresh_burst_hint.clone();
        move |_, state| {
            refresh_burst_hint(burst_buffering_switch.is_active(), state);
            low_memory_mode.set(state);
            glib::signal::Propagation::Proceed
        }
    });
    // The stored value lands a moment after the screen is built (and could change from
    // elsewhere): the switch follows it without re-triggering its own handler.
    low_memory_mode.add_listener({
        let low_memory_switch = low_memory_switch.clone();
        let burst_buffering_switch = burst_buffering_switch.clone();
        move |on| {
            if low_memory_switch.is_active() != on {
                low_memory_switch.block_signal(&low_memory_handler);
                low_memory_switch.set_active(on);
                low_memory_switch.set_state(on);
                low_memory_switch.unblock_signal(&low_memory_handler);
            }
            refresh_burst_hint(burst_buffering_switch.is_active(), on);
        }
    });

    page.add(&playback_group);

    let appearance_group = adw::PreferencesGroup::new();
    appearance_group.set_title(&tr("Appearance"));
    let theme_row = combo_row(&tr("Theme"), "", &[tr("System"), tr("Light"), tr("Dark")]);
    theme_row.set_selected(theme_index(theme));
    appearance_group.add(&theme_row);
    theme_row.connect_selected_notify({
        let pool = pool.clone();
        let toast_overlay = toast_overlay.clone();
        move |row| {
            let theme = theme_from_index(row.selected());
            crate::application::apply_theme(theme);
            let pool = pool.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                if let Err(err) = abs_core::settings::save_theme(&pool, theme).await {
                    crate::error_reporting::report_background_error(&toast_overlay, &tr("Saving the theme"), err);
                }
            });
        }
    });

    // Language: "System (<the language it resolves to>)" first, then every language the app has a
    // translation for (English always). The choice is stored right away but only applies at the
    // next start — screens already built keep their text — so the row says so, and a change that
    // would alter the language adds a toast.
    let language_codes = crate::i18n::available_languages();
    let mut language_items = vec![tr_args("System ({language})", &[("language", &crate::i18n::language_name(&crate::i18n::system_language()))])];
    language_items.extend(language_codes.iter().map(|code| crate::i18n::language_name(code)));
    let language_row = combo_row(&tr("Language"), &tr("Changes the next time the app starts"), &language_items);
    language_row.set_selected(language_index(&crate::i18n::selected_setting(), &language_codes));
    appearance_group.add(&language_row);
    language_row.connect_selected_notify({
        let pool = pool.clone();
        let toast_overlay = toast_overlay.clone();
        move |row| {
            let setting = language_setting(row.selected(), &language_codes);
            crate::i18n::set_selected_setting(&setting);
            if crate::i18n::language_for_setting(&setting) != crate::i18n::active_language() {
                toast_overlay.add_toast(adw::Toast::new(&tr("The language changes the next time the app starts")));
            }
            let pool = pool.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                if let Err(err) = abs_core::settings::save_language(&pool, &setting).await {
                    crate::error_reporting::report_background_error(&toast_overlay, &tr("Saving the language"), err);
                }
            });
        }
    });
    page.add(&appearance_group);

    // Diagnostics: the switch drives the process-wide log scrubber directly (logging is global,
    // so is its switch — see `crate::log_privacy::global`); the stored value was applied at
    // startup by `main.rs::setup`.
    let diagnostics_group = adw::PreferencesGroup::new();
    diagnostics_group.set_title(&tr("Diagnostics"));
    let anonymize_logs_switch = gtk4::Switch::new();
    anonymize_logs_switch.set_valign(gtk4::Align::Center);
    let anonymize_logs_on = crate::log_privacy::global().is_enabled();
    anonymize_logs_switch.set_state(anonymize_logs_on);
    anonymize_logs_switch.set_active(anonymize_logs_on);
    let anonymize_logs_row = adw::ActionRow::builder()
        .title(tr("Anonymize logs"))
        .subtitle(tr("Hides the server address, usernames and access tokens in log files. Turn off only while debugging."))
        .build();
    anonymize_logs_row.add_suffix(&anonymize_logs_switch);
    anonymize_logs_row.set_activatable_widget(Some(&anonymize_logs_switch));
    diagnostics_group.add(&anonymize_logs_row);
    anonymize_logs_switch.connect_state_set({
        let pool = pool.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, state| {
            let redactor = crate::log_privacy::global();
            // The audit line is written while anonymization is on, so it is never the line that
            // exposes anything.
            if state {
                redactor.set_enabled(true);
                tracing::info!("log anonymization turned on");
            } else {
                tracing::info!("log anonymization turned off");
                redactor.set_enabled(false);
            }
            let pool = pool.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                if let Err(err) = abs_core::settings::save_anonymize_logs(&pool, state).await {
                    crate::error_reporting::report_background_error(&toast_overlay, &tr("Saving the log setting"), err);
                }
            });
            glib::signal::Propagation::Proceed
        }
    });
    // A pseudo-setting: it only does something when activated. Logs are buffered, so flush first
    // — otherwise a fresh launch would open a file that doesn't have this session in it yet.
    let open_log_row = adw::ActionRow::builder()
        .title(tr("Open latest log file"))
        .subtitle(tr("Opens it in your default text viewer"))
        .activatable(true)
        .build();
    open_log_row.add_suffix(&gtk4::Image::from_icon_name("adw-external-link-symbolic"));
    open_log_row.connect_activated({
        let paths = paths.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| open_latest_log(&paths, &toast_overlay)
    });
    diagnostics_group.add(&open_log_row);
    page.add(&diagnostics_group);

    let about_group = adw::PreferencesGroup::new();
    about_group.set_title(&tr("About"));
    let about_row = adw::ActionRow::builder()
        .title(tr("About Audiobooklet"))
        // TRANSLATORS: {version} is the app's version number, e.g. "0.9.0".
        .subtitle(tr_args("Version {version}", &[("version", env!("CARGO_PKG_VERSION"))]))
        .build();
    about_row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    about_row.set_activatable(true);
    about_row.connect_activated({
        let window = window.clone();
        move |_| push_about(&window)
    });
    about_group.add(&about_row);
    page.add(&about_group);

    let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    content.append(&header);
    content.append(&page);
    toast_overlay.set_child(Some(&content));

    SettingsScreen {
        root: toast_overlay.clone().upcast(),
        #[cfg(test)]
        hooks: SettingsHooks {
            pause_on_unplug_switch,
            resume_on_replug_switch,
            default_speed_row,
            skip_back_row,
            skip_forward_row,
            wifi_only_switch,
            wifi_only_row,
            burst_buffering_switch,
            burst_buffering_row,
            low_memory_switch,
            anonymize_logs_switch,
            open_log_row,
            theme_row,
            language_row,
            about_row,
            account_row,
            server_rows: server_rows_hooks,
            add_server_row,
        },
    }
}

/// Hands the newest log file to the desktop's default handler for it (a text editor, via the
/// OpenURI portal when sandboxed). Failures are reported as toasts: nothing else on this page
/// would tell the user why a tap did nothing.
fn open_latest_log(paths: &AppPaths, toast_overlay: &adw::ToastOverlay) {
    crate::crash_reporting::flush_logs();
    match paths.latest_log_file() {
        Ok(Some(path)) => {
            let uri = gtk4::gio::File::for_path(&path).uri();
            if let Err(err) = crate::external_apps::launch_default_for_uri(&uri) {
                crate::error_reporting::report_background_error(toast_overlay, &tr("Opening the log file"), err);
            }
        }
        Ok(None) => toast_overlay.add_toast(adw::Toast::new(&tr("No log file yet"))),
        Err(err) => crate::error_reporting::report_background_error(toast_overlay, &tr("Finding the log file"), err),
    }
}

/// Pushes a server's Connection page by swapping the window's content — the same mechanism the
/// full player uses (`AdwNavigationView` is out of reach at this crate's libadwaita `v1_2`
/// ceiling). The page's back button restores the shell widget captured here, so no shell state
/// is lost.
fn push_connection(
    pool: &SqlitePool,
    server: &Server,
    account: Option<Account>,
    window: &adw::ApplicationWindow,
    on_session_changed: &Rc<dyn Fn()>,
) {
    let Some(shell_root) = window.content() else { return };
    let screen = crate::screens::connection::build(
        pool.clone(),
        server.clone(),
        account,
        window,
        &shell_root,
        on_session_changed.clone(),
    );
    crate::widgets::swap_content(window, &screen.root);
}

/// Shows the About screen the same way `push_connection` shows the Connection page: swapping
/// the main window's own content, with its own back button, rather than opening a second
/// top-level window. This used to be `adw::AboutWindow` — a genuine second `GtkWindow`, chosen
/// only because `AdwAboutDialog` needs libadwaita 1.5+, out of reach of this crate's v1.2
/// feature ceiling (`app/Cargo.toml`). A real Librem 5 field report found it had no reliable way
/// to be dismissed under Phosh's default compositor (phoc) — every *other* secondary screen in
/// this app already avoids opening a second window for exactly this class of reason, and About
/// was the one deliberate exception. It no longer is: see `docs/design/ui-spec.md`'s About
/// section.
pub(crate) fn push_about(window: &adw::ApplicationWindow) {
    let Some(shell_root) = window.content() else { return };

    let toast_overlay = adw::ToastOverlay::new();

    let back_button = gtk4::Button::builder().icon_name("go-previous-symbolic").tooltip_text(tr("Back")).css_classes(["flat"]).build();
    crate::a11y::name(&back_button, &tr("Back"));
    back_button.connect_clicked({
        let window = window.clone();
        let shell_root = shell_root.clone();
        move |_| crate::widgets::swap_content(&window, &shell_root)
    });

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&tr("About Audiobooklet"), "")));
    header.pack_start(&back_button);

    let page = adw::PreferencesPage::new();

    let info_group = adw::PreferencesGroup::new();
    let version_row = adw::ActionRow::builder()
        .title(crate::application::APP_NAME)
        // TRANSLATORS: {version} is the app's version number, e.g. "0.9.0".
        .subtitle(tr_args("Version {version}", &[("version", env!("CARGO_PKG_VERSION"))]))
        .build();
    info_group.add(&version_row);
    page.add(&info_group);

    let links_group = adw::PreferencesGroup::new();
    let website_row = adw::ActionRow::builder()
        .title(tr("Website"))
        .subtitle(WEBSITE_URL)
        .activatable(true)
        .build();
    website_row.add_suffix(&gtk4::Image::from_icon_name("adw-external-link-symbolic"));
    website_row.connect_activated({
        let toast_overlay = toast_overlay.clone();
        move |_| {
            // Firing the launch is all this needs to wait for — the default handler (a browser
            // via the desktop's URL-opening portal) runs as its own detached process, so there's
            // nothing further to await.
            if let Err(err) = crate::external_apps::launch_default_for_uri(WEBSITE_URL) {
                crate::error_reporting::report_background_error(&toast_overlay, &tr("Opening the website"), err);
            }
        }
    });
    links_group.add(&website_row);
    let license_row = adw::ActionRow::builder().title(tr("License")).subtitle("GNU General Public License v3.0").build(); // i18n: ignore
    links_group.add(&license_row);
    page.add(&links_group);

    let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    content.append(&header);
    content.append(&page);
    toast_overlay.set_child(Some(&content));

    crate::widgets::swap_content(window, &toast_overlay);
}

const WEBSITE_URL: &str = "https://github.com/gdr-aislop/audiobooklet";

/// A server URL without its scheme (`host[:port][/path]`) — what Account/Servers rows show instead
/// of the full URL (which stays on the server's Connection page).
pub(crate) fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    // A path is kept (`audiobooks.dev/audiobookshelf`): two servers on one host differ only there.
    rest.trim_end_matches('/')
}


/// The extra sentence a sign-out/remove confirmation carries when some listening progress on this
/// device hasn't reached the server yet — removing the rows would lose it for good.
fn unpushed_progress_warning(unpushed_books: i64) -> String {
    if unpushed_books <= 0 {
        return String::new();
    }
    let warning = ntr(
        "Listening progress for {count} book hasn't reached the server yet and will be lost.",
        "Listening progress for {count} books hasn't reached the server yet and will be lost.",
        u32::try_from(unpushed_books).unwrap_or(u32::MAX),
    );
    format!("\n\n{warning}")
}

/// A modal Ok/Cancel confirmation for a destructive session change — the same
/// `GtkMessageDialog` pattern the Welcome screen's replace-data confirmation uses. Cancel just
/// destroys the dialog; the confirmed action runs only on the affirmative response.
pub(crate) fn confirm(
    window: &adw::ApplicationWindow,
    heading: &str,
    body: &str,
    confirm_label: &str,
    on_confirm: Rc<dyn Fn()>,
) {
    let dialog = gtk4::MessageDialog::builder()
        .message_type(gtk4::MessageType::Warning)
        .text(heading)
        .secondary_text(body)
        .modal(true)
        .transient_for(window)
        .build();
    dialog.add_button(&tr("Cancel"), gtk4::ResponseType::Cancel);
    let confirm_button = dialog.add_button(confirm_label, gtk4::ResponseType::Ok);
    confirm_button.add_css_class("destructive-action");
    dialog.connect_response(move |dialog, response| {
        dialog.destroy();
        if response == gtk4::ResponseType::Ok {
            on_confirm();
        }
    });
    dialog.present();
}

fn speed_labels() -> Vec<String> {
    SPEED_PRESETS.iter().map(|speed| crate::screens::player::format_speed(*speed)).collect()
}

fn skip_labels() -> Vec<String> {
    // "sec" is the abbreviation for seconds; the plural form matters for languages whose number words differ by count.
    SKIP_CHOICES.iter().map(|seconds| ntr("{count} sec", "{count} sec", *seconds as u32)).collect()
}

/// Index of a speed within `SPEED_PRESETS`, falling back to the default speed's position when the
/// stored value isn't a preset (e.g. after a settings-format change) — the row must always show
/// something a preset means.
fn speed_index(speed: f64) -> u32 {
    SPEED_PRESETS
        .iter()
        .position(|preset| (*preset - speed).abs() < f64::EPSILON)
        .unwrap_or_else(|| SPEED_PRESETS.iter().position(|preset| (*preset - abs_core::playback::DEFAULT_SPEED).abs() < f64::EPSILON).unwrap_or(0)) as u32
}

/// Index of an interval within `SKIP_CHOICES`, falling back to `fallback`'s position when the
/// stored value isn't a choice — same reasoning as `speed_index`.
fn skip_index(seconds: i64, fallback: i64) -> u32 {
    SKIP_CHOICES
        .iter()
        .position(|choice| *choice == seconds)
        .unwrap_or_else(|| SKIP_CHOICES.iter().position(|choice| *choice == fallback).unwrap_or(0)) as u32
}

/// The Language row's position for a stored `setting`: 0 is "System", then one per available
/// language. A language that has since lost its catalog shows as "System", which is what it
/// resolves to.
fn language_index(setting: &str, codes: &[String]) -> u32 {
    codes.iter().position(|code| code == setting).map_or(0, |position| position as u32 + 1)
}

/// The inverse of [`language_index`]: the value to store for the row's `index`.
fn language_setting(index: u32, codes: &[String]) -> String {
    index
        .checked_sub(1)
        .and_then(|position| codes.get(position as usize))
        .cloned()
        .unwrap_or_else(|| crate::i18n::SYSTEM.to_string())
}

fn theme_index(theme: Theme) -> u32 {
    match theme {
        Theme::System => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    }
}

fn theme_from_index(index: u32) -> Theme {
    match index {
        1 => Theme::Light,
        2 => Theme::Dark,
        _ => Theme::System,
    }
}

/// The Wi-Fi-only row's subtitle. Without a way to detect the connection type the setting
/// never blocks anything, so the row says that instead of promising something it can't do.
fn wifi_only_subtitle(can_detect_metered: bool) -> String {
    if can_detect_metered {
        tr("Don't start downloads on metered connections")
    } else {
        tr("Can't detect the connection type on this device, so downloads aren't restricted")
    }
}

// Translated where shown (`tr(BURST_BUFFERING_SUBTITLE)`); `tr_noop` only marks them for extraction.
const BURST_BUFFERING_SUBTITLE: &str = tr_noop("Downloads ahead at full speed so the radio can idle. Turn off on a slow or capped connection.");
const BURST_BUFFERING_LOW_MEMORY_HINT: &str = tr_noop("Uses up to 64 MB of buffer; consider turning it off in low memory mode.");

/// The burst-buffering row's subtitle: with low memory mode on as well, it adds a hint — the
/// setting itself is left as the user has it.
fn burst_buffering_subtitle(burst_buffering: bool, low_memory: bool) -> String {
    if burst_buffering && low_memory {
        format!("{}\n{}", tr(BURST_BUFFERING_SUBTITLE), tr(BURST_BUFFERING_LOW_MEMORY_HINT))
    } else {
        tr(BURST_BUFFERING_SUBTITLE)
    }
}

/// Fire-and-forget persistence on the shared Tokio runtime — same shape as every other GTK
/// signal handler that touches the database (`PlayerController`'s progress writes, Welcome's
/// Connect button): capture everything up front, spawn, report on failure through the toast
/// overlay. Also applies the new values to the live controller and download manager
/// immediately, so a change takes effect without an app restart.
///
/// Saves are serialized through `pending_save`/`writer_running` — one snapshot slot plus at most
/// one in-flight writer task. Two concurrent `save_playback_settings` calls would interleave
/// their per-key `kv::set`s (the pool allows multiple connections), so a stale snapshot's
/// `pause_on_headphone_unplug=true` could land *after* a newer snapshot's `false` and win —
/// caught live by the gtk_fast scenario: the resume toggle's save overracing the pause toggle's
/// left the just-toggled value silently un-persisted. Draining the slot until empty also
/// coalesces a rapid toggle burst into at most one save per still-queued snapshot.
fn persist(
    pool: &SqlitePool,
    pending_save: &Rc<RefCell<Option<PlaybackSettings>>>,
    writer_running: &Rc<Cell<bool>>,
    settings: &RefCell<PlaybackSettings>,
    controller: &PlayerController,
    download_manager: &DownloadManager,
    toast_overlay: &adw::ToastOverlay,
) {
    let snapshot = *settings.borrow();
    controller.set_headphone_behavior(snapshot.pause_on_headphone_unplug, snapshot.resume_on_headphone_replug);
    controller.set_playback_config(
        snapshot.default_speed,
        snapshot.skip_back_seconds as f64,
        snapshot.skip_forward_seconds as f64,
    );
    controller.set_burst_buffering(snapshot.burst_buffering);
    download_manager.set_wifi_only(snapshot.wifi_only_downloads);
    *pending_save.borrow_mut() = Some(snapshot);
    if writer_running.get() {
        return;
    }
    writer_running.set(true);
    let pool = pool.clone();
    let pending_save = pending_save.clone();
    let writer_running = writer_running.clone();
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        loop {
            let Some(snapshot) = pending_save.borrow_mut().take() else { break };
            // The live controller already has the new values, so the change *looks* saved;
            // without a toast the user only learns otherwise on next launch.
            if let Err(err) = abs_core::settings::save_playback_settings(&pool, &snapshot).await {
                crate::error_reporting::report_background_error(&toast_overlay, &tr("Saving playback settings"), err);
            }
        }
        writer_running.set(false);
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::build;
    use adw::prelude::*;
    use std::time::Duration;

    use crate::player::tests::test_backend;
    use crate::test_support::pump_until;

    fn test_download_manager(pool: sqlx::SqlitePool) -> crate::downloads::DownloadManager {
        crate::downloads::DownloadManager::new(
            pool,
            crate::test_support::test_paths(),
            Box::new(abs_player::network_watch::UnknownNetworkMonitor),
            true,
        )
    }

    /// One active server/account — the minimum the signed-in shell (and therefore the Account
    /// group) requires. Returns the data `build` threads into the screen, in its shape.
    fn seed_active_session(
        runtime: &tokio::runtime::Runtime,
        pool: &sqlx::SqlitePool,
        url: &str,
        username: &str,
    ) -> (abs_storage::models::Server, Vec<abs_storage::models::Account>) {
        runtime.block_on(async {
            let server_id = abs_storage::repo::servers::add(pool, url).await.unwrap();
            let account_id = abs_storage::repo::accounts::add(pool, &server_id, username, "token", None).await.unwrap();
            abs_storage::repo::accounts::set_active(pool, &account_id).await.unwrap();
            let server = abs_storage::repo::servers::get(pool, &server_id).await.unwrap();
            let account = abs_storage::repo::accounts::get(pool, &account_id).await.unwrap();
            (server, vec![account])
        })
    }

    fn find_message_dialog() -> Option<gtk4::MessageDialog> {
        gtk4::Window::list_toplevels()
            .into_iter()
            .find_map(|window| window.downcast::<gtk4::MessageDialog>().ok())
    }

    /// Depth-first search for a button by label — how the Add-Server scenario reaches the
    /// Welcome screen's Cancel button, which belongs to a screen built inside
    /// `application::show_add_server` (no hooks cross that boundary).
    fn find_button_with_label(widget: &gtk4::Widget, label: &str) -> Option<gtk4::Button> {
        if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
            if button.label().is_some_and(|text| text == label) {
                return Some(button.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = find_button_with_label(&current, label) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point.
    pub(crate) fn run(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let screen = build(
            pool.clone(),
            controller.clone(),
            test_download_manager(pool.clone()),
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            adw::ApplicationWindow::builder().build(),
        );

        assert!(screen.hooks.pause_on_unplug_switch.state(), "pause-on-unplug defaults to on");
        assert!(!screen.hooks.resume_on_replug_switch.state(), "resume-on-replug defaults to off");
        assert!(screen.hooks.resume_on_replug_switch.is_sensitive(), "resume is offered while pause-on-unplug is on");

        // A real click delivers the `state-set` signal with the new state (GTK's own
        // `activate` doesn't toggle plain switches in this GTK version), running the app's
        // handler — live-controller update plus persistence — before the class handler applies
        // the state. Emit exactly that, on both switches.
        let _: bool = screen.hooks.resume_on_replug_switch.emit_by_name("state-set", &[&true]);
        let _: bool = screen.hooks.pause_on_unplug_switch.emit_by_name("state-set", &[&false]);
        assert!(!screen.hooks.pause_on_unplug_switch.state());
        assert!(screen.hooks.resume_on_replug_switch.state());
        assert!(!screen.hooks.resume_on_replug_switch.is_sensitive(), "with pause-on-unplug off, resume has nothing to act on");
        // 1s, not the 300ms this used to get away with: `persist` always saves the whole
        // `PlaybackSettings` struct regardless of which single field changed, and it has grown a
        // field since (`burst_buffering`) — one more sequential `kv::set` was enough to push a
        // real save past 300ms in this sandbox.
        pump_until(|| false, Duration::from_millis(1000));
        assert_eq!(controller.headphone_behavior(), (false, true), "switch activation must reach the live controller");

        let (pause_saved, resume_saved) = runtime.block_on(async {
            let settings = abs_core::settings::load_playback_settings(&pool).await.unwrap();
            (settings.pause_on_headphone_unplug, settings.resume_on_headphone_replug)
        });
        assert!(!pause_saved, "toggling pause-on-unplug must persist");
        assert!(resume_saved, "toggling resume-on-replug must persist");
    }

    /// The rows added with the Playback/Appearance/About groups: the combos and the Wi-Fi/burst-
    /// buffering switches start from the settings the shell was built with, reach the live
    /// controller/manager on change, persist (including the theme's own key), and the About row
    /// opens an about window.
    pub(crate) fn run_playback_defaults_theme_and_about(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let download_manager = test_download_manager(pool.clone());
        let window = adw::ApplicationWindow::builder().build();
        let screen = build(
            pool.clone(),
            controller.clone(),
            download_manager.clone(),
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            window.clone(),
        );
        // About's back button swaps back to whatever the window was showing before — needs a
        // real shell root in place for that to be meaningful, same as `push_connection`'s own
        // tests (`crate::screens::connection::tests`).
        window.set_content(Some(&screen.root));

        assert_eq!(screen.hooks.default_speed_row.selected(), 1, "1.0× is the second speed preset");
        assert_eq!(screen.hooks.skip_back_row.selected(), 2, "15 seconds is the third skip choice");
        assert_eq!(screen.hooks.skip_forward_row.selected(), 4, "30 seconds is the fifth skip choice");
        assert!(screen.hooks.wifi_only_switch.state(), "Wi-Fi-only downloads defaults to on");
        assert_eq!(
            screen.hooks.wifi_only_row.subtitle().as_deref(),
            Some("Can't detect the connection type on this device, so downloads aren't restricted"),
            "with no way to detect a metered connection, the row must not promise to restrict downloads"
        );
        assert!(screen.hooks.burst_buffering_switch.state(), "burst buffering defaults to on");
        assert_eq!(screen.hooks.theme_row.selected(), 0, "theme defaults to System");

        // `set_selected` is the property change the combo handlers listen to (a real user choice
        // emits exactly that); the switch goes through the same `state-set` emission a click
        // delivers.
        screen.hooks.default_speed_row.set_selected(3); // 1.5×
        screen.hooks.skip_back_row.set_selected(0); // 5 sec
        screen.hooks.skip_forward_row.set_selected(6); // 60 sec
        let _: bool = screen.hooks.wifi_only_switch.emit_by_name("state-set", &[&false]);
        let _: bool = screen.hooks.burst_buffering_switch.emit_by_name("state-set", &[&false]);
        screen.hooks.theme_row.set_selected(2); // Dark

        assert_eq!(controller.default_speed(), 1.5, "combo activation must reach the live controller");
        assert_eq!(controller.skip_intervals(), (5.0, 60.0), "skip choices must reach the live controller");
        assert!(!download_manager.wifi_only(), "the Wi-Fi switch must reach the download manager");
        assert!(!controller.burst_buffering(), "the burst-buffering switch must reach the live controller");
        assert_eq!(
            adw::StyleManager::default().color_scheme(),
            adw::ColorScheme::ForceDark,
            "the theme row must apply immediately"
        );

        // The saves are spawned futures on the GLib main context — pump until they land, same as
        // the headphone-switch scenario above (see its own comment on why 1s, not 300ms).
        pump_until(|| false, Duration::from_millis(1000));

        let (saved, saved_theme) = runtime.block_on(async {
            let settings = abs_core::settings::load_playback_settings(&pool).await.unwrap();
            let theme = abs_core::settings::load_theme(&pool).await.unwrap();
            (settings, theme)
        });
        assert_eq!(saved.default_speed, 1.5, "the default speed must persist");
        assert_eq!(saved.skip_back_seconds, 5, "skip back must persist");
        assert_eq!(saved.skip_forward_seconds, 60, "skip forward must persist");
        assert!(!saved.wifi_only_downloads, "the Wi-Fi switch must persist");
        assert!(!saved.burst_buffering, "the burst-buffering switch must persist");
        assert_eq!(saved_theme, abs_core::settings::Theme::Dark, "the theme must persist");

        // About: activating the row swaps the window's content to the About screen (not a
        // second window — see `push_about`'s doc comment for why) and shows a back button that
        // swaps back. `ActionRowExt::activate` — the row-level activation that emits `activated`
        // — not the ambiguous widget-level one.
        let shell_root = window.content().expect("the shell must already be showing");
        adw::prelude::ActionRowExt::activate(&screen.hooks.about_row);
        pump_until(
            || window.content().is_some_and(|c| c != shell_root && crate::test_support::any_label_reads(&c, "Audiobooklet")),
            Duration::from_secs(2),
        );
        let about_content = window.content().expect("the About screen should be showing");
        let back_button: gtk4::Button =
            crate::widgets::find_descendant(&about_content).expect("the About screen must have a back button");
        back_button.emit_clicked();
        pump_until(|| window.content().is_some_and(|c| c == shell_root), Duration::from_secs(2));
    }

    /// The Language row offers "System (<language>)" first — resolved to English here, since
    /// there are no other catalogs — then English; choosing one persists, survives reopening
    /// Settings, and (as English on an English system changes nothing) shows no restart toast.
    pub(crate) fn run_language_row_offers_system_and_english_and_persists(runtime: &tokio::runtime::Runtime) {
        let build_screen = |pool: &sqlx::SqlitePool, servers: Vec<(abs_storage::models::Server, Vec<abs_storage::models::Account>)>| {
            let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
            build(
                pool.clone(),
                controller,
                test_download_manager(pool.clone()),
                abs_core::settings::PlaybackSettings::default(),
                crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
                abs_core::settings::Theme::default(),
                crate::test_support::test_paths(),
                servers,
                adw::ApplicationWindow::builder().build(),
            )
        };
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let screen = build_screen(&pool, servers.clone());
        let row = &screen.hooks.language_row;

        assert_eq!(row.title(), "Language");
        let model = row.model().expect("the row has a model");
        let item = |position: u32| {
            model.item(position).and_downcast::<gtk4::StringObject>().map(|s| s.string().to_string())
        };
        assert_eq!(model.n_items(), 2, "System plus English — the only language with strings so far");
        assert_eq!(item(0).as_deref(), Some("System (English)"), "System names the language it resolves to");
        assert_eq!(item(1).as_deref(), Some("English"));
        assert_eq!(row.selected(), 0, "the language defaults to System");

        row.set_selected(1);
        pump_until(|| false, Duration::from_millis(500));
        assert_eq!(runtime.block_on(abs_core::settings::load_language(&pool)).unwrap(), "en", "the choice must persist");
        assert_eq!(crate::i18n::selected_setting(), "en");
        assert!(
            !crate::test_support::any_label_reads(&screen.root, "The language changes the next time the app starts"),
            "English on an English system changes nothing, so there is nothing to restart for"
        );

        let reopened = build_screen(&pool, servers);
        assert_eq!(reopened.hooks.language_row.selected(), 1, "reopening Settings must show the stored choice");

        reopened.hooks.language_row.set_selected(0);
        pump_until(|| false, Duration::from_millis(500));
        assert_eq!(runtime.block_on(abs_core::settings::load_language(&pool)).unwrap(), "system");
        assert_eq!(crate::i18n::selected_setting(), "system");
    }

    #[test]
    fn language_row_positions_map_to_settings_and_back() {
        let codes = vec!["en".to_string(), "de".to_string()];
        assert_eq!(super::language_index("system", &codes), 0);
        assert_eq!(super::language_index("en", &codes), 1);
        assert_eq!(super::language_index("de", &codes), 2);
        assert_eq!(super::language_index("fr", &codes), 0, "a language that lost its catalog reads as System");
        assert_eq!(super::language_setting(0, &codes), "system");
        assert_eq!(super::language_setting(1, &codes), "en");
        assert_eq!(super::language_setting(2, &codes), "de");
        assert_eq!(super::language_setting(9, &codes), "system", "an out-of-range position is System");
    }

    /// A theme or playback-setting change that applies live but fails to persist must say so,
    /// instead of silently being lost on the next launch. Closing the pool makes every write
    /// fail for real.
    pub(crate) fn run_failed_setting_saves_toast(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let download_manager = test_download_manager(pool.clone());
        let window = adw::ApplicationWindow::builder().build();
        let screen = build(
            pool.clone(),
            controller.clone(),
            download_manager,
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            window.clone(),
        );
        // `AdwToastOverlay` defers toasts while unmapped.
        window.set_content(Some(&screen.root));
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        runtime.block_on(pool.close());

        screen.hooks.theme_row.set_selected(2); // Dark
        pump_until(|| crate::test_support::any_label_reads(&screen.root, "Saving the theme failed — try again"), Duration::from_secs(5));
        assert!(
            crate::test_support::any_label_reads(&screen.root, "Saving the theme failed — try again"),
            "a theme that applied but didn't persist must be reported"
        );

        screen.hooks.default_speed_row.set_selected(3); // 1.5×
        assert_eq!(controller.default_speed(), 1.5, "the live change still applies");
        // 10 s, not 5: the overlay shows one toast at a time, so this one queues behind the
        // theme toast above.
        pump_until(
            || crate::test_support::any_label_reads(&screen.root, "Saving playback settings failed — try again"),
            Duration::from_secs(10),
        );
        assert!(
            crate::test_support::any_label_reads(&screen.root, "Saving playback settings failed — try again"),
            "a playback setting that applied but didn't persist must be reported"
        );

        crate::application::apply_theme(abs_core::settings::Theme::System);
        window.destroy();
    }

    /// The Account and Servers groups mirror the database: the active account's row (username,
    /// host · active), one row per server with its username and active marker, and per-server
    /// menu items whose sensitivity follows who's active. Includes the one action that doesn't
    /// destroy data — switching the active server — end to end.
    pub(crate) fn run_account_and_servers_rows_reflect_the_database(runtime: &tokio::runtime::Runtime) {
        // Two pools on one file: the rebuilt shell's background tasks (home/library syncs against
        // unreachable servers) hold the main pool's connections for a long time, so post-rebuild
        // asserts go through their own idle pool instead of contending with them.
        let (pool, assert_pool) = runtime.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let db_path = tmp.path().join("db.sqlite3");
            std::mem::forget(tmp);
            let pool = abs_storage::connect_and_migrate(&db_path).await.unwrap();
            let assert_pool = abs_storage::connect_and_migrate(&db_path).await.unwrap();
            (pool, assert_pool)
        });
        let mut servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        servers.push(seed_active_session_but_inactive(runtime, &pool, "http://127.0.0.1:2", "bob"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(
            pool.clone(),
            controller,
            test_download_manager(pool.clone()),
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            app_window.clone(),
        );
        let hooks = &screen.hooks;

        assert_eq!(hooks.account_row.title(), "jane");
        assert_eq!(hooks.account_row.subtitle().as_deref(), Some("127.0.0.1:1 · active"));

        assert_eq!(hooks.server_rows.len(), 2, "one row per configured server");
        assert_eq!(hooks.server_rows[0].row.title(), "127.0.0.1:1");
        assert_eq!(hooks.server_rows[0].row.subtitle().as_deref(), Some("jane · active"));
        assert_eq!(hooks.server_rows[1].row.title(), "127.0.0.1:2");
        assert_eq!(hooks.server_rows[1].row.subtitle().as_deref(), Some("bob"));

        assert!(
            !hooks.server_rows[0].switch_item.is_sensitive(),
            "the active server can't be switched to — it's already active"
        );
        assert!(hooks.server_rows[0].sign_out_item.is_sensitive() && hooks.server_rows[0].remove_item.is_sensitive());

        // Tapping the Account row's body pushes the active server's Connection page — a window
        // content swap, not a rebuild (the page's back button is covered by the connection
        // scenario).
        let old_root = screen.root.clone();
        app_window.set_content(Some(&old_root));
        adw::prelude::ActionRowExt::activate(&hooks.account_row);
        pump_until(
            {
                let old_root = old_root.clone();
                let app_window = app_window.clone();
                move || app_window.content().as_ref() != Some(&old_root)
            },
            Duration::from_secs(5),
        );

        // Switching to the second server: the DB's active account flips, and the shell the
        // settings screen lives in is rebuilt around the new session (here observed as the
        // window's content being swapped off the screen this build returned).
        let old_root = screen.root.clone();
        app_window.set_content(Some(&old_root));
        hooks.server_rows[1].switch_item.emit_clicked();
        pump_until(
            {
                let old_root = old_root.clone();
                let app_window = app_window.clone();
                move || app_window.content().as_ref() != Some(&old_root)
            },
            Duration::from_secs(10),
        );
        let active = runtime.block_on(abs_storage::repo::accounts::get_active(&assert_pool)).unwrap().unwrap();
        assert_eq!(active.username, "bob", "switching must make the second server's account active");
    }

    /// The two destructive menu actions, end to end: confirmed via the same MessageDialog
    /// pattern the Welcome screen's replacement confirmation uses, and both hand control back
    /// to `application::show_main_or_welcome` — signing out the only account lands on the
    /// first-run Welcome screen, and removing the server also purges its on-disk files.
    pub(crate) fn run_servers_menu_actions_rebuild_the_shell(runtime: &tokio::runtime::Runtime) {
        // --- Sign out: confirmed, then the account (and its local progress) is gone. ---
        {
            let pool = runtime.block_on(crate::test_support::pool());
            let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
            let (server_id, account_id) = (servers[0].0.id.clone(), servers[0].1[0].id.clone());
            let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
            let app_window = adw::ApplicationWindow::builder().build();
            let screen = build(
                pool.clone(),
                controller,
                test_download_manager(pool.clone()),
                abs_core::settings::PlaybackSettings::default(),
                crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
                abs_core::settings::Theme::default(),
                crate::test_support::test_paths(),
                servers,
                app_window.clone(),
            );
            let old_root = screen.root.clone();
            app_window.set_content(Some(&old_root));
            let dialog_body = || find_message_dialog().and_then(|dialog| dialog.secondary_text()).map(|text| text.to_string()).unwrap_or_default();

            screen.hooks.server_rows[0].sign_out_item.emit_clicked();
            pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
            assert!(!dialog_body().contains("hasn't reached the server"), "nothing unpushed, so no warning: {}", dialog_body());
            find_message_dialog().unwrap().response(gtk4::ResponseType::Cancel);
            pump_until(|| find_message_dialog().is_none(), Duration::from_secs(5));
            assert!(
                runtime.block_on(abs_storage::repo::accounts::get_active(&pool)).unwrap().is_some(),
                "a cancelled sign-out must leave the session intact"
            );

            // Progress listened to offline, not yet on the server: signing out would lose it, and
            // the confirmation must say so.
            runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server_id, "item-1", "Offline Book"));
            runtime.block_on(abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 42.0, false)).unwrap();
            screen.hooks.server_rows[0].sign_out_item.emit_clicked();
            pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
            assert!(
                dialog_body().contains("Listening progress for 1 book hasn't reached the server yet and will be lost."),
                "the sign-out confirmation must warn about unpushed progress: {}",
                dialog_body()
            );
            find_message_dialog().unwrap().response(gtk4::ResponseType::Ok);
            pump_until(
                {
                    let old_root = old_root.clone();
                    let app_window = app_window.clone();
                    move || app_window.content().as_ref() != Some(&old_root)
                },
                Duration::from_secs(10),
            );
            assert!(
                runtime.block_on(abs_storage::repo::accounts::get_active(&pool)).unwrap().is_none(),
                "a confirmed sign-out must remove the account — with no account left, the shell hands over to Welcome"
            );
        }

        // --- Remove server: confirmed, then rows cascade and the on-disk cache is purged. ---
        {
            let pool = runtime.block_on(crate::test_support::pool());
            let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
            let paths = crate::test_support::test_paths();
            let cover = paths.cover_cache_path(&servers[0].0.id, "item-1", "jpg");
            std::fs::create_dir_all(cover.parent().unwrap()).unwrap();
            std::fs::write(&cover, b"bytes").unwrap();

            let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
            let app_window = adw::ApplicationWindow::builder().build();
            let screen = build(
                pool.clone(),
                controller,
                test_download_manager(pool.clone()),
                abs_core::settings::PlaybackSettings::default(),
                crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
                abs_core::settings::Theme::default(),
                paths,
                servers,
                app_window.clone(),
            );
            let old_root = screen.root.clone();
            app_window.set_content(Some(&old_root));

            screen.hooks.server_rows[0].remove_item.emit_clicked();
            pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
            find_message_dialog().unwrap().response(gtk4::ResponseType::Ok);
            pump_until(
                {
                    let old_root = old_root.clone();
                    let app_window = app_window.clone();
                    move || app_window.content().as_ref() != Some(&old_root)
                },
                Duration::from_secs(10),
            );
            assert!(runtime.block_on(abs_storage::repo::servers::list(&pool)).unwrap().is_empty(), "the server row must be gone");
            assert!(!cover.exists(), "the server's on-disk cover cache must be purged, not orphaned");
        }
    }

    /// Found in the 2026-10-12 pass: signing out of the active account left no account active,
    /// and the app on the first-run login screen although another server still had an account.
    /// It now lands on that account; and the menu's Remove item is drawn so it can be read.
    pub(crate) fn run_signing_out_of_the_active_account_lands_on_another(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![
            seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane"),
            seed_active_session_but_inactive(runtime, &pool, "http://127.0.0.1:2", "bob"),
        ];
        let bob_id = servers[1].1[0].id.clone();
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(
            pool.clone(),
            controller,
            test_download_manager(pool.clone()),
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            app_window.clone(),
        );
        assert!(
            !screen.hooks.server_rows[0].remove_item.has_css_class("flat"),
            "flat + destructive-action draws the label invisibly in the popover"
        );
        let old_root = screen.root.clone();
        app_window.set_content(Some(&old_root));

        screen.hooks.server_rows[0].sign_out_item.emit_clicked();
        pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
        find_message_dialog().unwrap().response(gtk4::ResponseType::Ok);
        pump_until(
            {
                let old_root = old_root.clone();
                let app_window = app_window.clone();
                move || app_window.content().as_ref() != Some(&old_root)
            },
            Duration::from_secs(10),
        );
        // Asked from the main loop: the new shell's own startup work holds database connections
        // that only make progress while the loop runs, so blocking on the query here would stall.
        let active: std::rc::Rc<std::cell::RefCell<Option<Option<String>>>> = std::rc::Rc::default();
        gtk4::glib::spawn_future_local({
            let (pool, active) = (pool.clone(), active.clone());
            async move {
                let account = abs_storage::repo::accounts::get_active(&pool).await.unwrap();
                *active.borrow_mut() = Some(account.map(|a| a.id));
            }
        });
        pump_until(|| active.borrow().is_some(), Duration::from_secs(10));
        assert_eq!(active.borrow().clone().flatten(), Some(bob_id), "the remaining account takes over");
        let content = app_window.content().unwrap();
        assert!(find_button_with_label(&content, &crate::i18n::tr("Connect")).is_none(), "the main screen, not the login screen");
    }

    /// "Add Server" reuses the Welcome flow over the signed-in shell — and, unlike the first
    /// run, it must offer a way back: Cancel restores the shell exactly as it was (the captured
    /// root widget, not a rebuild).
    pub(crate) fn run_add_server_row_opens_welcome_and_cancels_back(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(
            pool.clone(),
            controller,
            test_download_manager(pool.clone()),
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            app_window.clone(),
        );
        let old_root = screen.root.clone();
        app_window.set_content(Some(&old_root));

        adw::prelude::ActionRowExt::activate(&screen.hooks.add_server_row);
        pump_until(
            {
                let old_root = old_root.clone();
                let app_window = app_window.clone();
                move || app_window.content().as_ref() != Some(&old_root)
            },
            Duration::from_secs(5),
        );
        let content = app_window.content().expect("the Add-Server flow swaps the window's content");
        // `is_visible` walks up to the toplevel — an unpresented window would make everything
        // report invisible, so map it first (same as the shell scenario's Ctrl+F focus check).
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));
        let cancel = find_button_with_label(&content, "Cancel")
            .expect("an Add-Server flow started from Settings must be cancelable — the shell is still behind it");
        assert!(cancel.is_visible());

        cancel.emit_clicked();
        pump_until(
            {
                let old_root = old_root.clone();
                let app_window = app_window.clone();
                move || app_window.content().as_ref() == Some(&old_root)
            },
            Duration::from_secs(10),
        );
    }

    /// `seed_active_session` for a *second* server: its account is created but never activated, the
    /// shape the Servers group must render for a server you're not currently using.
    fn seed_active_session_but_inactive(
        runtime: &tokio::runtime::Runtime,
        pool: &sqlx::SqlitePool,
        url: &str,
        username: &str,
    ) -> (abs_storage::models::Server, Vec<abs_storage::models::Account>) {
        runtime.block_on(async {
            let server_id = abs_storage::repo::servers::add(pool, url).await.unwrap();
            let account_id = abs_storage::repo::accounts::add(pool, &server_id, username, "token", None).await.unwrap();
            let server = abs_storage::repo::servers::get(pool, &server_id).await.unwrap();
            let account = abs_storage::repo::accounts::get(pool, &account_id).await.unwrap();
            (server, vec![account])
        })
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The Low memory mode switch defaults
    /// off, reaches the shared state and is saved; while it and burst buffering are both on the
    /// burst row says so (without changing the burst setting); and the switch follows the state.
    pub(crate) fn run_low_memory_mode_switch_persists_and_hints_at_burst_buffering(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let low_memory_mode = crate::low_memory_mode::LowMemoryModeState::new(pool.clone());
        let screen = build(
            pool.clone(),
            controller.clone(),
            test_download_manager(pool.clone()),
            abs_core::settings::PlaybackSettings::default(),
            low_memory_mode.clone(),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            adw::ApplicationWindow::builder().build(),
        );
        let hooks = &screen.hooks;
        let subtitle = || hooks.burst_buffering_row.subtitle().map(|s| s.to_string()).unwrap_or_default();

        assert!(!hooks.low_memory_switch.state(), "low memory mode defaults to off");
        assert!(!low_memory_mode.get());
        assert!(!subtitle().contains("low memory mode"), "no hint while it is off");

        let _: bool = hooks.low_memory_switch.emit_by_name("state-set", &[&true]);
        assert!(low_memory_mode.get(), "the switch reaches the shared state");
        assert!(subtitle().contains("consider turning it off in low memory mode"), "burst buffering is on too, so the row says so: {}", subtitle());
        assert!(controller.burst_buffering(), "the hint doesn't change the burst-buffering setting");

        let _: bool = hooks.burst_buffering_switch.emit_by_name("state-set", &[&false]);
        assert!(!subtitle().contains("low memory mode"), "no hint once burst buffering is off");
        let _: bool = hooks.burst_buffering_switch.emit_by_name("state-set", &[&true]);
        assert!(subtitle().contains("low memory mode"));

        pump_until(|| false, Duration::from_millis(500));
        assert!(runtime.block_on(abs_core::settings::load_low_memory_mode(&pool)).unwrap(), "the switch is saved");

        // Changed from elsewhere, the switch follows without re-triggering its own handler.
        low_memory_mode.set(false);
        assert!(!hooks.low_memory_switch.is_active());
        assert!(!subtitle().contains("low memory mode"), "the hint goes with it");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The "Anonymize logs" switch is on by
    /// default, drives the process-wide scrubber, and is saved.
    pub(crate) fn run_anonymize_logs_switch_drives_the_scrubber_and_persists(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let screen = build(
            pool.clone(),
            controller,
            test_download_manager(pool.clone()),
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            adw::ApplicationWindow::builder().build(),
        );
        let switch = &screen.hooks.anonymize_logs_switch;
        let redactor = crate::log_privacy::global();

        assert!(switch.state(), "anonymization defaults to on");
        assert!(redactor.is_enabled());

        let _: bool = switch.emit_by_name("state-set", &[&false]);
        assert!(!redactor.is_enabled(), "the switch reaches the scrubber");
        pump_until(|| false, Duration::from_millis(500));
        assert!(!runtime.block_on(abs_core::settings::load_anonymize_logs(&pool)).unwrap(), "off is saved");

        let _: bool = switch.emit_by_name("state-set", &[&true]);
        assert!(redactor.is_enabled());
        pump_until(|| false, Duration::from_millis(500));
        assert!(runtime.block_on(abs_core::settings::load_anonymize_logs(&pool)).unwrap(), "on is saved");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The "Open latest log file" row sits
    /// right under the anonymize switch; with no log yet, tapping it says so.
    pub(crate) fn run_open_latest_log_row_reports_a_missing_log(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let servers = vec![seed_active_session(runtime, &pool, "http://127.0.0.1:1", "jane")];
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let window = adw::ApplicationWindow::builder().build();
        let screen = build(
            pool.clone(),
            controller,
            test_download_manager(pool.clone()),
            abs_core::settings::PlaybackSettings::default(),
            crate::low_memory_mode::LowMemoryModeState::new(pool.clone()),
            abs_core::settings::Theme::default(),
            crate::test_support::test_paths(),
            servers,
            window.clone(),
        );
        window.set_content(Some(&screen.root));
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));
        let row = &screen.hooks.open_log_row;

        let switch_row = screen.hooks.anonymize_logs_switch.ancestor(adw::ActionRow::static_type()).expect("the switch lives in a row");
        assert_eq!(
            switch_row.next_sibling().as_ref(),
            Some(row.upcast_ref::<gtk4::Widget>()),
            "the log row sits directly below the anonymize row"
        );
        assert!(row.is_activatable());

        row.emit_by_name::<()>("activated", &[]);
        pump_until(|| crate::test_support::any_label_reads(&screen.root, "No log file yet"), Duration::from_secs(5));
        assert!(crate::test_support::any_label_reads(&screen.root, "No log file yet"), "no log, no pretending");
        window.destroy();
    }
}
