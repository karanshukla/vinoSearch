//! The files that make vinoSearch part of a desktop: the service's unit and
//! bus name, the launcher, and each search place's own entry (#45).

use crate::service::{flatpak, start};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use vinosearch_shared::dbus::{GNOME_PATH as GNOME, Names, RUNNER_PATH as OBJECT, names};
use vinosearch_shared::sandbox;
use vinosearch_shared::xdg::{config_home, data_home};

/// A place vinoSearch's results show (#45). Each has files of its own, written
/// and removed apart from the service's, so a Plasma session gets no GNOME
/// provider and a GNOME session no KRunner plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Place {
    /// KRunner (Alt+Space) and the Plasma app launcher's search, which reuses
    /// KRunner's plugins; they can't be split.
    Kde,
    /// GNOME Shell's overview search, from the next login.
    Gnome,
    /// The search window in the app menu, beside the settings window. Off
    /// unless turned on: no desktop gets it at install.
    Menu,
}

impl Place {
    pub const ALL: [Place; 3] = [Place::Kde, Place::Gnome, Place::Menu];

    fn name(self) -> &'static str {
        match self {
            Place::Kde => "KRunner and the Plasma launcher",
            Place::Gnome => "GNOME Shell",
            Place::Menu => "The search window in the app menu",
        }
    }
}

/// The places for the running desktop, from XDG_CURRENT_DESKTOP (`KDE`,
/// `ubuntu:GNOME`, ...). None for any other desktop (sway, Hyprland, COSMIC,
/// XFCE...): neither KRunner nor GNOME Shell runs there, so `vinosearch menu`
/// is the way in. Both when the variable is unset or empty (a TTY, ssh),
/// where the desktop can't be told, as every install did before #45.
fn places_for(desktop: Option<&str>) -> Vec<Place> {
    let desktop = desktop.unwrap_or_default();
    if desktop.trim().is_empty() {
        return vec![Place::Kde, Place::Gnome];
    }
    let names: Vec<String> = desktop.split(':').map(str::to_ascii_uppercase).collect();
    let has = |n: &str| names.iter().any(|d| d == n);
    [(Place::Kde, has("KDE")), (Place::Gnome, has("GNOME"))]
        .into_iter()
        .filter_map(|(p, on)| on.then_some(p))
        .collect()
}

pub fn current_places() -> Vec<Place> {
    places_for(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref())
}

/// GNOME Shell reads search providers from the system data dirs only, never
/// the user's, so the provider file goes in a directory of ours that
/// environment.d appends to XDG_DATA_DIRS, as Flatpak does for its per-user
/// exports. It takes effect from the next login.
struct GnomeFiles {
    provider: PathBuf,
    environment: PathBuf,
    xdg: PathBuf,
}

fn gnome_files() -> Result<GnomeFiles> {
    let data = data_home()?;
    let xdg = data.join("vinosearch/xdg");
    Ok(GnomeFiles {
        provider: data.join(&names().gnome_provider),
        environment: config_home()?.join("environment.d/60-vinosearch.conf"),
        xdg,
    })
}

/// Tell the desktop the app menu entries changed. Plasma reads them from its
/// ksycoca cache, which can go on launching a binary that's gone (a release
/// replaced by a source build) until it's rebuilt. Best effort: neither tool
/// is needed for the entries to work once the cache catches up.
fn refresh_menu() {
    let quiet = |program: &str, args: &[&std::ffi::OsStr]| {
        std::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if let Ok(data) = data_home() {
        quiet(
            "update-desktop-database",
            &["-q".as_ref(), data.join("applications").as_os_str()],
        );
    }
    let _ = quiet("kbuildsycoca6", &[]) || quiet("kbuildsycoca5", &[]);
}

fn launcher_path() -> Result<PathBuf> {
    Ok(data_home()?.join("applications").join(&names().desktop_id))
}

/// The app menu entry. It belongs to the install, not to a search place:
/// GNOME Shell also needs it to show the provider (a hidden entry disables
/// it), but turning GNOME off must not take vinoSearch out of Plasma's menu.
/// It opens the settings window when that is installed beside us, else the
/// documentation.
fn install_launcher() -> Result<PathBuf> {
    let launcher = launcher_path()?;
    let settings = std::env::current_exe()?.with_file_name("vinosearch-settings");
    let exec = if settings.is_file() {
        let s = utf8(&settings)?;
        if s.chars().any(char::is_control) {
            anyhow::bail!("{s:?}: cannot go in a desktop entry");
        }
        desktop_quote(s)
    } else {
        "xdg-open https://github.com/karanshukla/vinoSearch#readme".to_owned()
    };
    fs::create_dir_all(launcher.parent().unwrap())?;
    fs::write(&launcher, launcher_entry(names(), &exec, None))?;
    Ok(launcher)
}

/// The app menu entry, running `exec`. With `search`, the command that opens
/// the search window, which it offers as an action: the Flatpak's, in place
/// of a menu place of its own.
pub(crate) fn launcher_entry(n: &Names, exec: &str, search: Option<&str>) -> String {
    let mut entry = format!(
        "[Desktop Entry]\nType=Application\nName=vinoSearch\n\
         Comment=Semantic file search on the NPU\nIcon={}\nExec={exec}\n\
         StartupWMClass={}\nCategories=Utility;Settings;\nTerminal=false\n",
        n.icon, n.app_id
    );
    if let Some(search) = search {
        entry.push_str(&format!(
            "Keywords=find;files;photos;documents;\nActions=search;\n\n\
             [Desktop Action search]\nName=Search Files\nExec={search}\n"
        ));
    }
    entry
}

/// The search window's app menu entry. It needs vinosearch-settings beside
/// us. No StartupWMClass: the window's app id is the settings entry's.
fn install_search_entry() -> Result<()> {
    let entry = search_entry_path()?;
    let settings = std::env::current_exe()?.with_file_name("vinosearch-settings");
    if !settings.is_file() {
        bail!("{}: not installed", settings.display());
    }
    let s = utf8(&settings)?;
    if s.chars().any(char::is_control) {
        bail!("{s:?}: cannot go in a desktop entry");
    }
    fs::create_dir_all(entry.parent().unwrap())?;
    fs::write(&entry, search_entry(&desktop_quote(s), None))?;
    println!("wrote {}", entry.display());
    Ok(())
}

fn search_entry_path() -> Result<PathBuf> {
    Ok(data_home()?.join(&names().search_entry))
}

/// The search window's entry, `settings` being the command that opens the
/// settings window. With `only_with`, a program the menu shows the entry
/// only in the presence of: the Flatpak's launcher, so an entry left behind
/// by `flatpak uninstall` isn't offered.
pub(crate) fn search_entry(settings: &str, only_with: Option<&str>) -> String {
    let mut entry = format!(
        "[Desktop Entry]\nType=Application\nName=vinoSearch Search\n\
         Comment=Search your files by what's in them\nKeywords=find;files;photos;documents;\n\
         Icon={}\nExec={settings} --search\nCategories=Utility;\nTerminal=false\n",
        names().icon
    );
    if let Some(program) = only_with {
        entry.push_str(&format!("TryExec={program}\n"));
    }
    entry
}

/// GNOME Shell's search provider file.
pub(crate) fn gnome_provider(n: &Names) -> String {
    format!(
        "[Shell Search Provider]\nDesktopId={}\nBusName={}\nObjectPath={GNOME}\nVersion=2\n",
        n.desktop_id, n.service
    )
}

/// KRunner's D-Bus plugin file.
pub(crate) fn krunner_plugin(n: &Names) -> String {
    format!(
        "[Desktop Entry]\nName=vinoSearch\nComment=Semantic file search on the NPU\n\
         X-KDE-ServiceTypes=Plasma/Runner\nType=Service\nIcon={}\n\
         X-KDE-PluginInfo-Name={}\nX-KDE-PluginInfo-EnabledByDefault=true\n\
         X-Plasma-API=DBus\nX-Plasma-DBusRunner-Service={}\nX-Plasma-DBusRunner-Path={OBJECT}\n\
         X-Plasma-Request-Actions-Once=true\nX-Plasma-Runner-Min-Letter-Count=3\n\
         X-Plasma-Runner-Unique-Results=true\n",
        n.icon, n.krunner_plugin_id, n.service
    )
}

/// The D-Bus activation file, running `exec`, a command line already quoted.
pub(crate) fn dbus_service(n: &Names, exec: &str) -> String {
    format!("[D-BUS Service]\nName={}\nExec={exec}\n", n.service)
}

fn install_gnome() -> Result<()> {
    let g = gnome_files()?;
    let xdg = g.xdg.to_str().context("data directory is not UTF-8")?;
    if xdg.contains([':', '$', '\\', '\n']) {
        anyhow::bail!("{xdg}: cannot go in XDG_DATA_DIRS");
    }
    for p in [&g.provider, &g.environment] {
        fs::create_dir_all(p.parent().unwrap())?;
    }
    fs::write(&g.provider, gnome_provider(names()))?;
    let launcher = install_launcher()?;
    // Appended, so it never shadows anything the system provides. The
    // default is the XDG spec's, for sessions that leave the variable unset.
    fs::write(
        &g.environment,
        format!("XDG_DATA_DIRS=${{XDG_DATA_DIRS:-/usr/local/share/:/usr/share/}}:{xdg}\n"),
    )?;
    println!(
        "wrote {}\nwrote {}\nwrote {} (GNOME Shell search: from the next login)",
        g.provider.display(),
        launcher.display(),
        g.environment.display()
    );
    Ok(())
}

fn install_kde() -> Result<()> {
    let plugin = data_home()?.join(&names().krunner_plugin);
    fs::create_dir_all(plugin.parent().unwrap())?;
    fs::write(&plugin, krunner_plugin(names()))?;
    println!("wrote {}", plugin.display());
    restart_krunner();
    Ok(())
}

/// KRunner reads its plugins at start, so it is restarted when it is
/// running: asked to quit, as `kquitapp6 krunner` does, then started by its
/// bus name, and the launcher's search stops (or starts) asking us at once.
/// Quiet when it isn't running or there is no session bus.
fn restart_krunner() {
    const KRUNNER: &str = "org.kde.krunner";
    let quit = (
        "/MainApplication",
        "org.qtproject.Qt.QCoreApplication",
        "quit",
    );
    match start::stop(KRUNNER, quit).map(|was| was && start::start(KRUNNER).is_ok()) {
        Ok(true) => println!("restarted KRunner"),
        Ok(false) => {}
        Err(e) => eprintln!("restarting KRunner: {e:#}"),
    }
}

/// Write `place`'s files. The service is installed too if it isn't yet,
/// with no flags: a plugin pointing at a bus name nothing activates would
/// search nothing.
pub fn install_place(place: Place) -> Result<()> {
    if !data_home()?.join(&names().dbus_service_file).exists() {
        install_service(None, None, None)?;
    }
    match place {
        Place::Kde => install_kde(),
        Place::Gnome => install_gnome(),
        Place::Menu => install_search_entry(),
    }?;
    refresh_menu();
    Ok(())
}

/// Remove `place`'s files; the service stays for the other place, the CLI
/// and the settings app.
pub fn uninstall_place(place: Place) -> Result<()> {
    let files = match place {
        Place::Kde => vec![data_home()?.join(&names().krunner_plugin)],
        Place::Gnome => {
            let g = gnome_files()?;
            vec![g.provider, g.environment]
        }
        Place::Menu => vec![search_entry_path()?],
    };
    let mut removed = 0;
    for p in files {
        if p.exists() {
            fs::remove_file(&p)?;
            println!("removed {}", p.display());
            removed += 1;
        }
    }
    if removed == 0 {
        println!("{} was already off", place.name());
    }
    if place == Place::Kde {
        restart_krunner();
    }
    refresh_menu();
    Ok(())
}

/// Turn one place on or off (`vinosearch integrate kde|gnome [--off]`).
pub fn integrate(place: Place, on: bool) -> Result<()> {
    if sandbox::flatpak_id().is_some() {
        return flatpak::integrate(place, on);
    }
    if on {
        install_place(place)?;
    } else {
        uninstall_place(place)?;
    }
    println!("{}: {}", place.name(), if on { "on" } else { "off" });
    Ok(())
}

/// The vinoPilot landing page's vinoSearch mark, with the viewBox cropped to
/// a 2 px margin: the page's 4 px left the tile visibly smaller than other
/// apps' icons in a task manager.
pub(crate) const ICON_SVG: &str = include_str!("../../data/dev.vinosearch.svg");

fn icon_path() -> Result<PathBuf> {
    Ok(data_home()?.join(icon_file(names())))
}

pub(crate) fn icon_file(n: &Names) -> String {
    format!("icons/hicolor/scalable/apps/{}.svg", n.icon)
}

/// Install the service and the search places for the running desktop;
/// `integrate` adds or removes one.
pub fn install(
    openvino_dir: Option<&Path>,
    device: Option<&str>,
    idle_unload_minutes: Option<f64>,
) -> Result<()> {
    if sandbox::flatpak_id().is_some() {
        let mut args = Vec::new();
        if let Some(d) = device {
            args.extend(["--device".into(), d.to_owned()]);
        }
        if let Some(m) = idle_unload_minutes {
            args.extend(["--idle-unload-minutes".into(), m.to_string()]);
        }
        return flatpak::install(&args);
    }
    install_service(openvino_dir, device, idle_unload_minutes)?;
    let places = current_places();
    if places.is_empty() {
        println!(
            "no KRunner or GNOME Shell in this session ({}): search with `vinosearch menu`, \
             bound to a key; `vinosearch integrate kde|gnome` adds either anyway",
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
        );
    }
    for place in places {
        match place {
            Place::Kde => install_kde()?,
            Place::Gnome => install_gnome()?,
            Place::Menu => install_search_entry()?,
        }
    }
    refresh_menu();
    Ok(())
}

/// The icon, the D-Bus activation file and the autostart entry, which
/// starts the runner at login so the watcher keeps the index current from
/// then on (D-Bus activation alone would start it only at the first query).
/// Then the runner is started, or restarted to run this binary. Only flags
/// given here are baked into the service; the rest come from the config file
/// each time the runner starts.
fn install_service(
    openvino_dir: Option<&Path>,
    device: Option<&str>,
    idle_unload_minutes: Option<f64>,
) -> Result<()> {
    let mut args = vec![utf8(&std::env::current_exe()?)?.to_owned()];
    if let Some(d) = device {
        args.extend(["--device".into(), d.to_owned()]);
    }
    if let Some(m) = idle_unload_minutes {
        args.extend(["--idle-unload-minutes".into(), m.to_string()]);
    }
    if let Some(d) = openvino_dir {
        args.extend(["--openvino-dir".into(), utf8(d)?.to_owned()]);
    }
    args.push("krunner".into());
    for a in &args {
        if a.chars().any(char::is_control) {
            anyhow::bail!("{a:?}: cannot go in a service's command line");
        }
    }
    let data = data_home()?;
    let service = data.join(&names().dbus_service_file);
    let autostart = start::autostart_path()?;
    let icon = icon_path()?;
    for p in [&service, &autostart, &icon] {
        fs::create_dir_all(p.parent().unwrap())?;
    }
    fs::write(&icon, ICON_SVG)?;
    let launcher = install_launcher()?;
    fs::write(
        &service,
        dbus_service(names(), &command_line(&args, dbus_quote)),
    )?;
    fs::write(
        &autostart,
        start::autostart_entry(&command_line(&args, desktop_quote)),
    )?;
    println!(
        "wrote {}\nwrote {}\nwrote {}\nwrote {}",
        service.display(),
        autostart.display(),
        icon.display(),
        launcher.display()
    );
    if let Some(unit) = start::remove_old_unit()? {
        println!(
            "removed {} (the autostart entry starts it now)",
            unit.display()
        );
    }
    restart_runner();
    Ok(())
}

/// Start the runner from the files just written, stopping one already
/// running. Reports rather than fails: without a session bus here (a TTY,
/// ssh) it starts at the next login.
fn restart_runner() {
    let restarted = start::stop_runner().and_then(|was| {
        start::start(&names().service)?;
        Ok(was)
    });
    let service = &names().service;
    match restarted {
        Ok(true) => println!("restarted {service}"),
        Ok(false) => println!("started {service}"),
        Err(e) => println!("not started now ({e:#}); it starts at the next login"),
    }
    if let Ok(log) = start::log_where() {
        println!("its log: {log}");
    }
}

fn utf8(p: &Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("{}: not UTF-8", p.display()))
}

/// `args` as one command line, each quoted by `quote`, so a path with a
/// space stays one argument.
fn command_line(args: &[String], quote: fn(&str) -> String) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// A double-quoted word in a D-Bus service file's Exec (dbus-daemon's and
/// dbus-broker's shell-like parsing: inside double quotes a backslash
/// escapes `\`, `"`, `$` and `` ` ``).
fn dbus_quote(arg: &str) -> String {
    let mut q = String::from('"');
    for c in arg.chars() {
        if matches!(c, '\\' | '"' | '$' | '`') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

/// A quoted argument in a desktop entry's Exec: the D-Bus quoting, which
/// matches the Exec key's, then `%%` for `%` (field codes) and every
/// backslash doubled again for the string value's own escaping.
fn desktop_quote(arg: &str) -> String {
    dbus_quote(arg).replace('\\', "\\\\").replace('%', "%%")
}

/// Everything off: both places, the service, the icon, the app menu entry.
/// The systemd unit an older install wrote goes too.
pub fn uninstall() -> Result<()> {
    if let Some(app) = sandbox::flatpak_id() {
        return flatpak::uninstall(app);
    }
    let data = data_home()?;
    if let Some(unit) = start::remove_old_unit()? {
        println!("removed {}", unit.display());
    }
    let service = &names().service;
    match start::stop_runner() {
        Ok(true) => println!("stopped {service}"),
        Ok(false) => {}
        Err(e) => eprintln!("stopping {service}: {e:#}"),
    }
    for place in Place::ALL {
        uninstall_place(place)?;
    }
    for p in [
        data.join(&names().dbus_service_file),
        start::autostart_path()?,
        icon_path()?,
        launcher_path()?,
    ] {
        if p.exists() {
            fs::remove_file(&p)?;
            println!("removed {}", p.display());
        }
    }
    refresh_menu();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_entry_opens_the_search_window() {
        let entry = search_entry(&desktop_quote("/home/u/my bin/vinosearch-settings"), None);
        assert!(
            entry.contains("\nExec=\"/home/u/my bin/vinosearch-settings\" --search\n"),
            "{entry}"
        );
        assert!(!entry.contains("StartupWMClass"), "{entry}");
        assert!(!entry.contains("TryExec"), "{entry}");
        // The Flatpak's goes from the menu with its launcher.
        let entry = search_entry("flatpak run a.b", Some("/x/exports/bin/a.b"));
        assert!(entry.ends_with("\nTryExec=/x/exports/bin/a.b\n"), "{entry}");
    }

    #[test]
    fn the_running_desktop_picks_the_places() {
        assert_eq!(places_for(Some("KDE")), [Place::Kde]);
        assert_eq!(places_for(Some("ubuntu:GNOME")), [Place::Gnome]);
        assert_eq!(places_for(Some("GNOME-Classic:GNOME")), [Place::Gnome]);
        assert_eq!(places_for(Some("sway")), []);
        assert_eq!(places_for(Some("Hyprland")), []);
        assert_eq!(places_for(Some("COSMIC")), []);
        assert_eq!(places_for(Some("XFCE")), []);
        assert_eq!(places_for(None), [Place::Kde, Place::Gnome]);
        assert_eq!(places_for(Some("")), [Place::Kde, Place::Gnome]);
    }

    #[test]
    fn exec_lines_quote_every_argument() {
        let args = ["/opt/my apps/vinosearch".to_owned(), "krunner".to_owned()];
        assert_eq!(
            command_line(&args, dbus_quote),
            r#""/opt/my apps/vinosearch" "krunner""#
        );
        let odd = r#"/a "b" \c $d `e` 100%"#;
        assert_eq!(dbus_quote(odd), r#""/a \"b\" \\c \$d \`e\` 100%""#);
        assert_eq!(
            desktop_quote(odd),
            r#""/a \\"b\\" \\\\c \\$d \\`e\\` 100%%""#
        );
    }
}
