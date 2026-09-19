//! The tray (Windows) and menu-bar status item (macOS): open and quit, plus a
//! quick switch — every shared display to one host, or one display at a time —
//! without opening any window.

use std::sync::Mutex;

use tauri::{
    ipc::Channel,
    menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    AppHandle, Emitter, Listener, Manager, Wry,
};

use super::{
    build_host_switcher_state, read_settings, report_failure_if_allowed, run_host_switch,
    show_main_window, ui_text, AppRuntime, HostSwitcherMonitor, ACTIVE_ROUTE_CHANGED_EVENT,
    HOST_NAMES_CHANGED_EVENT, HOST_ORDER_CHANGED_EVENT, MONITOR_IDENTITIES_CHANGED_EVENT,
    PEER_INPUTS_CHANGED_EVENT,
};

pub(super) const TRAY_ID: &str = "muxsu";
const OPEN_ID: &str = "tray-open";
const QUIT_ID: &str = "tray-quit";
/// Menu ids carry the display and host they switch, split by a character
/// neither a display key nor a host id contains.
const ID_SEPARATOR: char = '\n';
const SWITCH_ONE_PREFIX: &str = "tray-one";
const SWITCH_ALL_PREFIX: &str = "tray-all";

/// Whatever changes a host's name, order, port or the host a display shows
/// changes the menu too.
const MENU_EVENTS: [&str; 5] = [
    ACTIVE_ROUTE_CHANGED_EVENT,
    HOST_ORDER_CHANGED_EVENT,
    HOST_NAMES_CHANGED_EVENT,
    PEER_INPUTS_CHANGED_EVENT,
    MONITOR_IDENTITIES_CHANGED_EVENT,
];

/// What the menu showed when it was last built. The main window asks for a
/// rebuild on every refresh, and replacing a menu that is open closes it on
/// Windows, so a rebuild that would change nothing is skipped.
static LAST_MENU: Mutex<Option<String>> = Mutex::new(None);

enum TrayAction {
    Open,
    Quit,
    SwitchOne {
        monitor_key: String,
        host_id: String,
    },
    SwitchAll {
        host_id: String,
    },
}

fn parse_action(id: &str) -> Option<TrayAction> {
    match id {
        OPEN_ID => return Some(TrayAction::Open),
        QUIT_ID => return Some(TrayAction::Quit),
        _ => {}
    }
    let mut parts = id.split(ID_SEPARATOR);
    match (parts.next()?, parts.next(), parts.next(), parts.next()) {
        (SWITCH_ONE_PREFIX, Some(monitor_key), Some(host_id), None) => {
            Some(TrayAction::SwitchOne {
                monitor_key: monitor_key.to_owned(),
                host_id: host_id.to_owned(),
            })
        }
        (SWITCH_ALL_PREFIX, Some(host_id), None, None) => Some(TrayAction::SwitchAll {
            host_id: host_id.to_owned(),
        }),
        _ => None,
    }
}

fn switch_one_id(monitor_key: &str, host_id: &str) -> String {
    format!("{SWITCH_ONE_PREFIX}{ID_SEPARATOR}{monitor_key}{ID_SEPARATOR}{host_id}")
}

fn switch_all_id(host_id: &str) -> String {
    format!("{SWITCH_ALL_PREFIX}{ID_SEPARATOR}{host_id}")
}

/// Displays a switch to `host_id` would change: those that have a port for it
/// and are not already showing it.
fn switch_targets<'a>(
    monitors: &'a [HostSwitcherMonitor],
    host_id: &str,
) -> Vec<&'a HostSwitcherMonitor> {
    monitors
        .iter()
        .filter(|monitor| {
            monitor
                .hosts
                .iter()
                .any(|host| host.id == host_id && host.available && !host.is_active)
        })
        .collect()
}

fn current_monitors(app: &AppHandle) -> Vec<HostSwitcherMonitor> {
    let state = app.state::<AppRuntime>();
    match read_settings(&state) {
        Ok(settings) => build_host_switcher_state(&state, &settings).monitors,
        Err(error) => {
            tracing::warn!(error = %error, "unable to read settings for the tray menu");
            Vec::new()
        }
    }
}

/// Menu text as the platform draws it: Windows reads `&` as the marker of a
/// keyboard accelerator, so a host called "R&D" would lose its ampersand.
fn label(text: &str) -> String {
    if cfg!(target_os = "windows") {
        text.replace('&', "&&")
    } else {
        text.to_owned()
    }
}

/// Everything the menu shows, in the language it shows it in.
fn signature(monitors: &[HostSwitcherMonitor]) -> String {
    format!("{}\u{0}{monitors:?}", ui_text("zh-TW", "en"))
}

/// The hosts of one display as check items; the host on screen is checked.
fn host_items(
    app: &AppHandle,
    monitor: &HostSwitcherMonitor,
) -> tauri::Result<Vec<tauri::menu::CheckMenuItem<Wry>>> {
    monitor
        .hosts
        .iter()
        .map(|host| {
            CheckMenuItemBuilder::new(label(&host.name))
                .id(switch_one_id(&monitor.monitor_key, &host.id))
                .checked(host.is_active)
                .enabled(host.available && !host.is_active)
                .build(app)
        })
        .collect()
}

/// The menu for the tray as it is created; later changes go through
/// `refresh_menu`.
pub(super) fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let monitors = current_monitors(app);
    let menu = menu_for(app, &monitors)?;
    remember(Some(signature(&monitors)));
    Ok(menu)
}

fn remember(shown: Option<String>) {
    match LAST_MENU.lock() {
        Ok(mut last) => *last = shown,
        Err(poisoned) => *poisoned.into_inner() = shown,
    }
}

fn is_showing(shown: &str) -> bool {
    match LAST_MENU.lock() {
        Ok(last) => last.as_deref() == Some(shown),
        Err(_) => false,
    }
}

fn menu_for(app: &AppHandle, monitors: &[HostSwitcherMonitor]) -> tauri::Result<Menu<Wry>> {
    let mut menu = MenuBuilder::new(app);
    match monitors {
        [] => {}
        [monitor] => {
            let heading =
                MenuItemBuilder::new(ui_text("共用螢幕切到", "Switch the shared display to"))
                    .enabled(false)
                    .build(app)?;
            menu = menu.item(&heading);
            for item in host_items(app, monitor)? {
                menu = menu.item(&item);
            }
            menu = menu.separator();
        }
        [first, ..] => {
            let heading = MenuItemBuilder::new(ui_text("全部螢幕切到", "Switch every display to"))
                .enabled(false)
                .build(app)?;
            menu = menu.item(&heading);
            // Every display lists the same hosts in the same saved order.
            for host in &first.hosts {
                let showing_everywhere = monitors.iter().all(|monitor| {
                    monitor
                        .hosts
                        .iter()
                        .any(|option| option.id == host.id && option.is_active)
                });
                let item = CheckMenuItemBuilder::new(label(&host.name))
                    .id(switch_all_id(&host.id))
                    .checked(showing_everywhere)
                    .enabled(!switch_targets(monitors, &host.id).is_empty())
                    .build(app)?;
                menu = menu.item(&item);
            }
            menu = menu.separator();
            for monitor in monitors {
                let mut submenu = SubmenuBuilder::new(app, label(&monitor.name));
                for item in host_items(app, monitor)? {
                    submenu = submenu.item(&item);
                }
                menu = menu.item(&submenu.build()?);
            }
            menu = menu.separator();
        }
    }
    menu.text(OPEN_ID, ui_text("開啟 MuxSU", "Open MuxSU"))
        .separator()
        .text(QUIT_ID, ui_text("結束 MuxSU", "Quit MuxSU"))
        .build()
}

/// Rebuilds the menu from the saved settings when what it shows has changed.
/// Cheap: it reads no display.
pub(super) fn refresh_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let monitors = current_monitors(app);
    let shown = signature(&monitors);
    if is_showing(&shown) {
        return;
    }
    match menu_for(app, &monitors) {
        Ok(menu) => match tray.set_menu(Some(menu)) {
            Ok(()) => remember(Some(shown)),
            Err(error) => tracing::warn!(error = %error, "unable to update the tray menu"),
        },
        Err(error) => tracing::warn!(error = %error, "unable to build the tray menu"),
    }
}

/// Runs switches the menu asked for, one display at a time: the first switch
/// wakes a sleeping host, so later ones find it ready. Nothing on screen waits
/// for these, so a failure is logged and reported like any other.
fn run_switches(app: AppHandle, jobs: Vec<(String, String)>) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppRuntime>();
        let mut switched = false;
        for (monitor_key, host_id) in jobs {
            // No window follows the progress of a switch made from the menu.
            let progress = Channel::new(|_| Ok(()));
            match run_host_switch(monitor_key, host_id, progress, &state).await {
                Ok(_) => switched = true,
                Err(message) => {
                    tracing::warn!(error = %message, "a switch from the tray menu failed");
                    report_failure_if_allowed(&app, &message);
                }
            }
        }
        // Clicking a check item flips its tick on the spot, whatever the switch
        // then does, so the menu is rebuilt even when nothing changed.
        remember(None);
        refresh_menu(&app);
        if switched {
            // Every window shows which host is active.
            if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a switch");
            }
        }
    });
}

pub(super) fn handle_menu_event(app: &AppHandle, id: &str) {
    match parse_action(id) {
        Some(TrayAction::Open) => show_main_window(app),
        Some(TrayAction::Quit) => app.exit(0),
        Some(TrayAction::SwitchOne {
            monitor_key,
            host_id,
        }) => run_switches(app.clone(), vec![(monitor_key, host_id)]),
        Some(TrayAction::SwitchAll { host_id }) => {
            let monitors = current_monitors(app);
            let jobs = switch_targets(&monitors, &host_id)
                .into_iter()
                .map(|monitor| (monitor.monitor_key.clone(), host_id.clone()))
                .collect();
            run_switches(app.clone(), jobs);
        }
        None => {}
    }
}

/// Keeps the menu current with changes made here or by a paired host.
pub(super) fn follow_changes(app: &AppHandle) {
    for event in MENU_EVENTS {
        let handle = app.clone();
        app.listen_any(event, move |_| refresh_menu(&handle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_round_trip() {
        match parse_action(&switch_one_id("DMO/3410/DEMO-001", "peer-1")) {
            Some(TrayAction::SwitchOne {
                monitor_key,
                host_id,
            }) => {
                assert_eq!(monitor_key, "DMO/3410/DEMO-001");
                assert_eq!(host_id, "peer-1");
            }
            _ => panic!("expected a single-display switch"),
        }
        match parse_action(&switch_all_id("local")) {
            Some(TrayAction::SwitchAll { host_id }) => assert_eq!(host_id, "local"),
            _ => panic!("expected an every-display switch"),
        }
    }

    #[test]
    fn unknown_or_malformed_ids_do_nothing() {
        assert!(parse_action("tray-something").is_none());
        assert!(
            parse_action(&format!("{SWITCH_ONE_PREFIX}{ID_SEPARATOR}only-a-display")).is_none()
        );
        assert!(parse_action(&format!(
            "{SWITCH_ALL_PREFIX}{ID_SEPARATOR}a{ID_SEPARATOR}b"
        ))
        .is_none());
    }
}
