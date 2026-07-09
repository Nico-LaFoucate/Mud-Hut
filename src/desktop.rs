// SPDX-License-Identifier: Apache-2.0
//! Generate a freedesktop `.desktop` launcher (+ XDG MIME associations + icon) for
//! an installed app, so it shows up in the application menu and opens its file types
//! via `neutron launch <app> --prefix <prefix> %f`. Everything here is best-effort —
//! a missing `update-desktop-database`/`xdg-mime`/`neutron` never fails the install.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use base64::Engine;

use crate::catalog::App;
use crate::output::Emitter;

/// Write `~/.local/share/applications/mudhut-<id>.desktop` for `app` installed in
/// `prefix`, save its icon, register MIME associations, and refresh the desktop DB.
pub fn install_entry(em: &Emitter, app: &App, prefix: &Path) -> Result<PathBuf> {
    let apps_dir = data_home().join("applications");
    std::fs::create_dir_all(&apps_dir)
        .with_context(|| format!("creating {}", apps_dir.display()))?;
    let file = apps_dir.join(format!("mudhut-{}.desktop", app.id));

    // Best-effort real icon (falls back to the theme name mudhut-<id> if unavailable).
    let icon = save_icon(app, prefix).unwrap_or_else(|| format!("mudhut-{}", app.id));

    let prefix_s = prefix.display().to_string();
    // Exec: neutron launch <app> --prefix <p> [%f]. %f is the opened file (MIME assoc).
    let exec = if app.mime.is_empty() {
        format!("neutron launch {} --prefix \"{}\"", app.id, prefix_s)
    } else {
        format!("neutron launch {} --prefix \"{}\" %f", app.id, prefix_s)
    };
    let mime_line = if app.mime.is_empty() {
        String::new()
    } else {
        format!("MimeType={};\n", app.mime.join(";"))
    };

    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=Adobe {name}\n\
         GenericName={name}\n\
         Comment=Adobe {name} on Neutron\n\
         Exec={exec}\n\
         Icon={icon}\n\
         Terminal=false\n\
         Categories={cats}\n\
         {mime}\
         StartupNotify=true\n\
         X-Mudhut-Prefix={prefix_s}\n",
        name = app.name,
        cats = app.categories,
        mime = mime_line,
    );
    std::fs::write(&file, &contents).with_context(|| format!("writing {}", file.display()))?;

    // Best-effort DB refresh + default-handler registration.
    let _ = Command::new("update-desktop-database").arg(&apps_dir).status();
    for m in app.mime {
        let _ = Command::new("xdg-mime")
            .args(["default", &format!("mudhut-{}.desktop", app.id), m])
            .status();
    }
    em.note(&format!("desktop entry: {}", file.display()));
    Ok(file)
}

/// Ask `neutron` for the app's icon (a `data:image/png;base64,...` URI), decode it,
/// and save it under the hicolor icon theme. Returns the installed icon name on
/// success. All-or-nothing best-effort (returns None on any hiccup).
fn save_icon(app: &App, prefix: &Path) -> Option<String> {
    let out = Command::new("neutron")
        .args(["--json", "apps", "--prefix"])
        .arg(prefix)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let uri = v
        .get("apps")?
        .as_array()?
        .iter()
        .find(|a| a.get("id").and_then(|i| i.as_str()) == Some(app.id))?
        .get("icon")?
        .as_str()?;
    let b64 = uri.strip_prefix("data:image/png;base64,")?;
    let png = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;

    let dir = data_home().join("icons/hicolor/256x256/apps");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!("mudhut-{}", app.id);
    std::fs::write(dir.join(format!("{name}.png")), &png).ok()?;
    Some(name)
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
    })
}
