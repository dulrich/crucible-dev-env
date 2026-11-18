//! Publishing the greeter profile: on every config apply the user session writes
//! `<dir>/profile.kdl` (and the background image copy) into the greeter's profile directory,
//! because the greeter user cannot read the user's home. The pure builder [`publish_to`] takes the
//! directory as an argument (tests use a tempdir); [`publish_for_env`] is the I/O edge that
//! `apply_config` calls. With no `greeter` block, or a directory that is absent or not writable,
//! nothing is written and nothing is logged above `debug!`.

use std::io;
use std::path::{Path, PathBuf};

use crucible_config::{Config, GreeterProfile, PROFILE_FILE, SurfaceBackground, write_profile};
use tracing::{debug, info, warn};

use crate::lock_config::{wallpaper_entries, wallpaper_state_path_from};
use crate::terminal_theme::write_if_changed;

/// The system directory the greeter reads its profile from.
pub const GREETER_DIR: &str = "/var/lib/cde-greeter";
/// The stock background the deploy installs where the greeter user can read it.
pub const STOCK_IMAGE: &str = "/opt/cde/share/backgrounds/stock.png";
/// Present in the directory iff the background is `user`; `wallpaper-rotate.sh` checks it.
pub const MARKER_FILE: &str = "user-background";

/// The first image of a wallpaper state file: the path of its first entry (the state file's one
/// parser, [`wallpaper_entries`]). **Pure.**
#[must_use]
pub fn first_state_image(state_text: &str) -> Option<PathBuf> {
    wallpaper_entries(state_text)
        .first()
        .map(|(_, path)| PathBuf::from(path))
}

/// Copies `src` to `<dir>/background.<ext of src>` atomically, skipping equal bytes. Returns the
/// copy's path.
fn copy_background(dir: &Path, src: &Path) -> io::Result<PathBuf> {
    let ext = src
        .extension()
        .map_or_else(|| "img".to_string(), |e| e.to_string_lossy().into_owned());
    let dest = dir.join(format!("background.{ext}"));
    let bytes = std::fs::read(src)?;
    if std::fs::read(&dest).is_ok_and(|old| old == bytes) {
        return Ok(dest);
    }
    let tmp = dir.join(format!(".background.{ext}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &dest).inspect_err(|_| {
        std::fs::remove_file(&tmp).ok();
    })?;
    Ok(dest)
}

/// Publishes the profile for `config` into `dir`. Nothing happens (`Ok(false)`) without a
/// `greeter` block or when `dir` is not a directory. `state_text` is the wallpaper state (for a
/// `user` background) and `stock` the stock image path. Returns whether `profile.kdl` changed.
///
/// # Errors
/// Any I/O error writing the profile, the image copy or the marker.
pub fn publish_to(
    dir: &Path,
    config: &Config,
    state_text: Option<&str>,
    stock: &Path,
) -> io::Result<bool> {
    let Some(greeter) = config.greeter.as_ref() else {
        return Ok(false);
    };
    if !dir.is_dir() {
        return Ok(false);
    }
    let marker = dir.join(MARKER_FILE);
    let image = match greeter.background {
        SurfaceBackground::Color(_) => None,
        SurfaceBackground::Stock => Some(stock.to_path_buf()),
        SurfaceBackground::User => match state_text.and_then(first_state_image) {
            Some(src) => match copy_background(dir, &src) {
                Ok(copy) => Some(copy),
                Err(err) => {
                    warn!(src = %src.display(), %err, "cannot copy the greeter background");
                    None
                }
            },
            None => None,
        },
    };
    if greeter.background == SurfaceBackground::User {
        if !marker.exists() {
            std::fs::write(&marker, b"")?;
        }
    } else if let Err(err) = std::fs::remove_file(&marker)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }
    let profile = GreeterProfile::from_config(
        greeter,
        config.theme.as_ref(),
        config.overlay.alpha,
        config.idle.blank_after,
        &config.keyboard,
        &config.binds,
        image.as_deref(),
    );
    write_if_changed(&dir.join(PROFILE_FILE), &write_profile(&profile))
}

/// Publishes `config` into [`GREETER_DIR`] from the process environment. A failure only warns
/// (a permission problem only `debug!`s); never fails the config apply.
pub fn publish_for_env(config: &Config) {
    if config.greeter.is_none() {
        return;
    }
    let dir = Path::new(GREETER_DIR);
    let state = wallpaper_state_path_from(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .and_then(|p| std::fs::read_to_string(p).ok());
    match publish_to(dir, config, state.as_deref(), Path::new(STOCK_IMAGE)) {
        Ok(true) => info!(dir = %dir.display(), "greeter profile published"),
        Ok(false) => {}
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            debug!(dir = %dir.display(), %err, "greeter directory not writable");
        }
        Err(err) => warn!(dir = %dir.display(), %err, "cannot publish the greeter profile"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_config::{GreeterConfig, Rgb};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cde-publish-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn config(bg: Option<SurfaceBackground>) -> Config {
        Config {
            greeter: bg.map(|background| GreeterConfig {
                background,
                ..GreeterConfig::default()
            }),
            ..Config::default()
        }
    }

    #[test]
    fn first_image_of_both_state_forms() {
        assert_eq!(first_state_image("/a/b.png\n"), Some("/a/b.png".into()));
        assert_eq!(
            first_state_image("DP-1\t/a/c.jpg\nDP-2\t/a/d.jpg\n"),
            Some("/a/c.jpg".into())
        );
        assert_eq!(first_state_image("*\t/a/e.png"), Some("/a/e.png".into()));
        assert_eq!(first_state_image(""), None);
    }

    #[test]
    fn first_image_follows_the_state_file_parser_on_malformed_input() {
        // Blank means trims to nothing; tab fields are verbatim; a bare line is trimmed.
        for (text, want) in [
            ("  \n/a.png\n", Some("/a.png")),
            (" /a.png \n", Some("/a.png")),
            ("\t/a.png\n/b.png\n", Some("/b.png")),
            ("*\t\n/b.png\n", Some("/b.png")),
            ("DP-1\t\n/b.png\n", Some("/b.png")),
            ("DP-1\t /a b.png\n", Some(" /a b.png")),
            ("\t\n", None),
        ] {
            assert_eq!(first_state_image(text), want.map(PathBuf::from), "{text:?}");
        }
    }

    #[test]
    fn no_greeter_block_or_dir_writes_nothing() {
        let d = tmp("none");
        assert!(!publish_to(&d, &config(None), None, Path::new("/s.png")).unwrap());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        let c = config(Some(SurfaceBackground::Stock));
        assert!(!publish_to(&d.join("absent"), &c, None, Path::new("/s.png")).unwrap());
        assert!(!d.join("absent").exists());
    }

    #[test]
    fn color_background_has_no_image_and_no_marker() {
        let d = tmp("color");
        std::fs::write(d.join(MARKER_FILE), b"").unwrap();
        let c = config(Some(SurfaceBackground::Color(Rgb(1, 2, 3))));
        assert!(publish_to(&d, &c, None, Path::new("/s.png")).unwrap());
        let text = std::fs::read_to_string(d.join(PROFILE_FILE)).unwrap();
        assert!(text.contains("#010203") && !text.contains("image"));
        assert!(!d.join(MARKER_FILE).exists());
        // Unchanged: skipped.
        assert!(!publish_to(&d, &c, None, Path::new("/s.png")).unwrap());
    }

    #[test]
    fn stock_points_at_the_shared_png() {
        let d = tmp("stock");
        let c = config(Some(SurfaceBackground::Stock));
        let stock = Path::new("/opt/cde/share/backgrounds/stock.png");
        publish_to(&d, &c, None, stock).unwrap();
        let text = std::fs::read_to_string(d.join(PROFILE_FILE)).unwrap();
        assert!(text.contains("/opt/cde/share/backgrounds/stock.png"));
        assert!(!d.join(MARKER_FILE).exists());
    }

    #[test]
    fn user_copies_the_first_image_and_sets_the_marker() {
        let d = tmp("user");
        let src = tmp("user-src").join("wall.jpg");
        std::fs::write(&src, b"jpegbytes").unwrap();
        let state = format!("DP-1\t{}\nDP-2\t/other.png\n", src.display());
        let c = config(Some(SurfaceBackground::User));
        assert!(publish_to(&d, &c, Some(&state), Path::new("/s.png")).unwrap());
        assert_eq!(
            std::fs::read(d.join("background.jpg")).unwrap(),
            b"jpegbytes"
        );
        assert!(d.join(MARKER_FILE).exists());
        let text = std::fs::read_to_string(d.join(PROFILE_FILE)).unwrap();
        assert!(text.contains(&d.join("background.jpg").display().to_string()));
    }

    #[test]
    fn user_without_state_falls_back_to_black_with_marker() {
        let d = tmp("user-none");
        let c = config(Some(SurfaceBackground::User));
        publish_to(&d, &c, None, Path::new("/s.png")).unwrap();
        let text = std::fs::read_to_string(d.join(PROFILE_FILE)).unwrap();
        assert!(text.contains("#000000") && !text.contains("image"));
        assert!(d.join(MARKER_FILE).exists());
    }
}
