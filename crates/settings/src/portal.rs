//! A folder chooser through xdg-desktop-portal's FileChooser, so the dialog is
//! the desktop's own (Dolphin's on Plasma, Nautilus's on GNOME) and egui needs
//! no file dialog of its own.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_lite::StreamExt;
use zbus::{Connection, Proxy};
use zbus::{OwnedVariant, Variant};

const PORTAL: &str = "org.freedesktop.portal.Desktop";

/// Blocks until the dialog closes. None when it was cancelled.
pub fn pick_folder(title: &str) -> Result<Option<PathBuf>> {
    zbus::block_on(pick(title))
}

/// Where the portal will answer: it derives the Request object's path from
/// our bus name and the token, so listening there before asking means a fast
/// answer can't arrive ahead of the match rule.
fn request_path(conn: &Connection, token: &str) -> Result<String> {
    let sender = conn
        .unique_name()
        .context("no unique bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    Ok(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{token}"
    ))
}

async fn pick(title: &str) -> Result<Option<PathBuf>> {
    let conn = Connection::session().await?;
    let token = format!("vinosearch{}", std::process::id());
    let request = Proxy::new(
        &conn,
        PORTAL,
        request_path(&conn, &token)?,
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut responses = request.receive_signal("Response").await?;
    let chooser = Proxy::new(
        &conn,
        PORTAL,
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.FileChooser",
    )
    .await?;
    let options: HashMap<&str, Variant> = HashMap::from([
        ("handle_token", Variant::from(token.as_str())),
        ("directory", Variant::from(true)),
        ("modal", Variant::from(true)),
    ]);
    let _: zbus::OwnedObjectPath = chooser
        .call("OpenFile", &("", title, options))
        .await
        .context("no desktop file chooser portal is available")?;
    let message = responses
        .next()
        .await
        .context("the file chooser portal closed unexpectedly")?;
    let (code, results): (u32, HashMap<String, OwnedVariant>) = message.body().deserialize()?;
    if code != 0 {
        return Ok(None);
    }
    let uri = match results.get("uris").map(|v| &**v) {
        Some(Variant::Array(a)) => a.iter().find_map(|v| match v {
            Variant::Str(s) => Some(s.to_string()),
            _ => None,
        }),
        _ => None,
    };
    Ok(uri.as_deref().and_then(file_uri_path))
}

/// `file:///a%20b` as `/a b`. Anything but a local file URI is None.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let out: Vec<u8> = percent_encoding::percent_decode_str(rest).collect();
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode_to_paths() {
        assert_eq!(
            file_uri_path("file:///home/u/My%20Notes"),
            Some(PathBuf::from("/home/u/My Notes"))
        );
        assert_eq!(
            file_uri_path("file://localhost/tmp/%C3%A9t%C3%A9"),
            Some(PathBuf::from("/tmp/été"))
        );
        assert_eq!(
            file_uri_path("file:///a/100%"),
            Some(PathBuf::from("/a/100%"))
        );
        assert_eq!(file_uri_path("https://example.com/x"), None);
        assert_eq!(file_uri_path("file://host/x"), None);
    }

    #[test]
    fn a_percent_followed_by_two_hex_digits_is_a_byte() {
        assert_eq!(
            file_uri_path("file:///a%2Fb%7e"),
            Some(PathBuf::from("/a/b~"))
        );
    }

    #[test]
    fn a_percent_not_followed_by_two_hex_digits_stays_as_written() {
        for uri_path in ["/a%+f", "/a%-1", "/a%2", "/a%zz", "/a%"] {
            assert_eq!(
                file_uri_path(&format!("file://{uri_path}")),
                Some(PathBuf::from(uri_path))
            );
        }
    }
}
