//! The service inside the Flatpak. Its search places and D-Bus activation
//! file are static files the manifest installs and Flatpak exports
//! (`export`, from the templates a native install writes). Flatpak marks
//! exported search providers off by default (KRunner's
//! `EnabledByDefault=false`, GNOME Shell's `DefaultDisabled=true`), so a
//! place is turned on and off in the desktop's own setting, which its search
//! settings page shows too: KRunner's `krunnerrc` and GNOME's
//! `org.gnome.desktop.search-providers`, written on the host. Starting at
//! login goes through the Background portal, which writes and removes the
//! autostart entry on the host.

use crate::service::krunner::{self, Place};
use crate::service::start;
use anyhow::{Context, Result, bail};
use futures_lite::StreamExt;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use vinosearch_shared::dbus::{Names, names};
use vinosearch_shared::sandbox;
use zbus::{Connection, OwnedVariant, Proxy, Variant};

const PORTAL: &str = "org.freedesktop.portal.Desktop";

/// Write the files the Flatpak exports for app id `app` under `share`
/// (`/app/share` at build time). Exec lines name commands inside the app;
/// Flatpak rewrites them to `flatpak run` when it exports them.
pub fn export(share: &Path, app: &str) -> Result<()> {
    let n = Names::flatpak(app);
    let files = [
        (
            format!("applications/{}", n.desktop_id),
            krunner::launcher_entry(
                &n,
                "vinosearch-settings",
                Some("vinosearch-settings --search"),
            ),
        ),
        (krunner::icon_file(&n), krunner::ICON_SVG.to_owned()),
        (
            n.dbus_service_file.clone(),
            krunner::dbus_service(&n, "vinosearch krunner"),
        ),
        (n.krunner_plugin.clone(), krunner::krunner_plugin(&n)),
        (n.gnome_provider.clone(), krunner::gnome_provider(&n)),
    ];
    for (path, text) in files {
        let path = share.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// `krunner --install` in the Flatpak: the running desktop's search place
/// on, start at login through the Background portal, and the service
/// started now. `args` go before `krunner` on the autostart entry's command
/// line.
pub fn install(args: &[String]) -> Result<()> {
    let mut command = vec!["vinosearch".to_owned()];
    command.extend(args.iter().cloned());
    command.push("krunner".into());
    match request_background(Some(&command)) {
        Ok(()) => println!("starts at login (the Background portal wrote its autostart entry)"),
        Err(e) => println!("not started at login: {e:#}"),
    }
    remember_asked(true);
    for place in krunner::current_places() {
        if let Err(e) = set_place(place, true) {
            println!("{e:#}");
        }
    }
    let service = &names().service;
    match start::start(service) {
        Ok(()) => println!("started {service}"),
        Err(e) => println!("not started now ({e:#})"),
    }
    print_log();
    Ok(())
}

/// `krunner --uninstall` in the Flatpak: every search place off, no start
/// at login, and the service stopped. Its files stay until the Flatpak is
/// uninstalled, but nothing starts it.
pub fn uninstall(app: &str) -> Result<()> {
    let service = &names().service;
    match start::stop_runner() {
        Ok(true) => println!("stopped {service}"),
        Ok(false) => {}
        Err(e) => eprintln!("stopping {service}: {e:#}"),
    }
    for place in [Place::Kde, Place::Gnome] {
        if let Err(e) = set_place(place, false) {
            eprintln!("{e:#}");
        }
    }
    match request_background(None) {
        Ok(()) => println!("no longer starts at login"),
        Err(e) => eprintln!("still starts at login: {e:#}"),
    }
    remember_asked(false);
    println!("flatpak uninstall {app} removes the rest, with --delete-data its index and models");
    Ok(())
}

/// `integrate` in the Flatpak.
pub fn integrate(place: Place, on: bool) -> Result<()> {
    set_place(place, on)
}

/// Turn `place` on or off in the desktop's setting for it.
fn set_place(place: Place, on: bool) -> Result<()> {
    let n = names();
    match place {
        Place::Kde => {
            let key = format!("{}Enabled", n.krunner_plugin_id);
            // Off is the key gone, as Flatpak's export leaves the plugin off
            // by default. --notify: KRunner rereads its plugins when the
            // group changes.
            let value = if on { "true" } else { "--delete" };
            let wrote = ["kwriteconfig6", "kwriteconfig5"].into_iter().any(|tool| {
                sandbox::host_output(&[
                    tool,
                    "--notify",
                    "--file",
                    "krunnerrc",
                    "--group",
                    "Plugins",
                    "--key",
                    &key,
                    value,
                ])
                .is_some()
            });
            if !wrote {
                bail!("KRunner's setting wasn't written: no kwriteconfig6 on the host");
            }
            println!(
                "KRunner and the Plasma launcher: {}",
                if on { "on" } else { "off" }
            );
        }
        Place::Gnome => {
            let id = &n.desktop_id;
            let enabled = sandbox::gnome_providers("enabled");
            if !on && !enabled.as_ref().is_some_and(|l| l.contains(id)) {
                // Never turned on here: nothing to write, or nowhere to.
                println!("GNOME Shell: off");
                return Ok(());
            }
            let enabled = enabled
                .context("GNOME Shell's setting wasn't read: no gsettings or schema on the host")?;
            let disabled = sandbox::gnome_providers("disabled").unwrap_or_default();
            let set = |key: &str, list: Vec<String>| {
                sandbox::host_output(&[
                    "gsettings",
                    "set",
                    sandbox::GNOME_SEARCH,
                    key,
                    &sandbox::strv(&list),
                ])
                .with_context(|| format!("GNOME Shell's {key} providers weren't written"))
            };
            let without = |list: Vec<String>| list.into_iter().filter(|p| p != id).collect();
            if on {
                let mut list: Vec<String> = without(enabled);
                list.push(id.clone());
                set("enabled", list)?;
                if disabled.contains(id) {
                    set("disabled", without(disabled))?;
                }
            } else {
                set("enabled", without(enabled))?;
            }
            println!("GNOME Shell: {}", if on { "on" } else { "off" });
        }
        Place::Menu if on => {
            println!("the search window is Search Files, an action of vinoSearch's app menu entry")
        }
        Place::Menu => bail!(
            "in the Flatpak the search window is an action of vinoSearch's own app menu entry, \
             so it can't be removed alone"
        ),
    }
    Ok(())
}

fn print_log() {
    if let Ok(log) = start::log_path() {
        println!("its log: {}", log.display());
    }
}

/// Note that start at login was asked for, or turned off, so the settings
/// window doesn't ask again on its own.
fn remember_asked(on: bool) {
    let Some(path) = sandbox::autostart_asked_path() else {
        return;
    };
    let written = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&path, if on { "on\n" } else { "off\n" }));
    if let Err(e) = written {
        eprintln!("{}: {e}", path.display());
    }
}

/// Ask the Background portal to run `command` at login, or with None to
/// stop doing so. Fails when the portal refuses or the user says no.
fn request_background(command: Option<&[String]>) -> Result<()> {
    zbus::block_on(async {
        let conn = Connection::session().await?;
        // The answer comes on a Request object named from our bus name and
        // this token; listening there first means it can't be missed.
        let token = format!("vinosearch{}", std::process::id());
        let sender = conn
            .unique_name()
            .context("no unique bus name")?
            .trim_start_matches(':')
            .replace('.', "_");
        let request = Proxy::new(
            &conn,
            PORTAL,
            format!("/org/freedesktop/portal/desktop/request/{sender}/{token}"),
            "org.freedesktop.portal.Request",
        )
        .await?;
        let mut responses = request.receive_signal("Response").await?;
        let background = Proxy::new(
            &conn,
            PORTAL,
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Background",
        )
        .await?;
        let mut options: HashMap<&str, Variant> = HashMap::from([
            ("handle_token", Variant::from(token.as_str())),
            (
                "reason",
                Variant::from("Keeps the search index current and answers searches"),
            ),
            ("autostart", Variant::from(command.is_some())),
        ]);
        if let Some(command) = command {
            options.insert("commandline", Variant::from(command.to_vec()));
        }
        let _: zbus::OwnedObjectPath = background
            .call("RequestBackground", &("", options))
            .await
            .context("the desktop has no Background portal")?;
        let message = responses.next().await.context("the portal went away")?;
        let (code, results): (u32, HashMap<String, OwnedVariant>) = message.body().deserialize()?;
        let granted = |key: &str| {
            results
                .get(key)
                .and_then(|v| bool::try_from(v.try_clone().ok()?).ok())
                .unwrap_or(false)
        };
        if code != 0 {
            bail!("the portal answered {code}");
        }
        if command.is_some() && !granted("background") {
            bail!("the desktop didn't allow vinoSearch to run in the background");
        }
        if command.is_some() && !granted("autostart") {
            bail!("the desktop didn't allow vinoSearch to start at login");
        }
        Ok(())
    })
}
