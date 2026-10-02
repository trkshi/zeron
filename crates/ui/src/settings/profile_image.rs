//! Device-local avatar copies. Persist the setting before retiring an old image.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use gpui::App;
use image::ImageDecoder as _;
use zeron_proto::{UserProfile, WorkspaceScope};

use super::{SavePolicy, SettingsStore, current, replace};

const IMAGE_DIR: &str = "profile-images";
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const AVATAR_SIDE: u32 = 256;
const INVALID_IMAGE: &str = "This image is unsupported, damaged, or too large. Choose a smaller PNG, JPEG, WebP, GIF, BMP, or TIFF image.";
const SAVE_ERROR: &str =
    "Unable to save the profile image. Check folder permissions and try again.";

pub(crate) fn account_key(
    scope: Option<WorkspaceScope>,
    user: Option<&UserProfile>,
) -> Option<String> {
    match scope {
        Some(WorkspaceScope::Local) => Some("local".into()),
        Some(WorkspaceScope::Development) => Some(match user {
            Some(user) => format!("development:{}", user.id),
            None => "development".into(),
        }),
        Some(WorkspaceScope::Synced) | None => user
            .filter(|user| !user.id.is_empty())
            .map(|user| format!("user:{}", user.id)),
    }
}

pub(crate) fn path(account_key: &str, cx: &App) -> Option<PathBuf> {
    cx.try_global::<SettingsStore>()?
        .current
        .profile_images_by_account
        .get(account_key)
        .cloned()
}

pub(crate) struct PreparedImage(Option<PathBuf>);

impl Drop for PreparedImage {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn read_image(source: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(source)
        .map_err(|_| "Unable to read this image. Choose an accessible file.".to_string())?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err("Choose an image file, not a folder or device.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Unable to read this image. Choose an accessible file.".to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("This image is larger than 20 MB. Choose a smaller image.".into());
    }
    Ok(bytes)
}

fn thumbnail(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let decode = || -> image::ImageResult<Vec<u8>> {
        let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(MAX_DECODE_BYTES);
        reader.limits(limits.clone());
        let mut decoder = reader.into_decoder()?;
        limits.reserve(decoder.total_bytes())?;
        decoder.set_limits(limits)?;
        let orientation = decoder
            .orientation()
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut decoded = image::DynamicImage::from_decoder(decoder)?;
        decoded.apply_orientation(orientation);
        if decoded.width() == 0 || decoded.height() == 0 {
            return Err(image::ImageError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Empty image",
            )));
        }
        // Keep one small, metadata-free frame; animated avatars never reach GPUI.
        let thumbnail = image::DynamicImage::ImageRgba8(
            decoded
                .resize_to_fill(
                    AVATAR_SIDE,
                    AVATAR_SIDE,
                    image::imageops::FilterType::Lanczos3,
                )
                .to_rgba8(),
        );
        let mut png = Cursor::new(Vec::new());
        thumbnail.write_to(&mut png, image::ImageFormat::Png)?;
        Ok(png.into_inner())
    };
    decode().map_err(|_| INVALID_IMAGE.to_string())
}

/// Reading, decoding, and preparing the copy belong on a background executor.
pub(crate) fn prepare(source: &Path, data_dir: &Path) -> Result<PreparedImage, String> {
    let bytes = thumbnail(&read_image(source)?)?;
    let directory = data_dir.join(IMAGE_DIR);
    std::fs::create_dir_all(&directory).map_err(|_| SAVE_ERROR.to_string())?;
    // Unique paths avoid GPUI showing a cached image after a replacement.
    let destination = directory.join(format!("profile-image-{}.png", uuid::Uuid::new_v4()));
    let temporary = destination.with_extension("png.tmp");
    if std::fs::write(&temporary, &bytes)
        .and_then(|_| std::fs::rename(&temporary, &destination))
        .is_err()
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(SAVE_ERROR.into());
    }
    Ok(PreparedImage(Some(destination)))
}

pub(crate) fn install(
    account_key: String,
    mut prepared: PreparedImage,
    cx: &mut App,
) -> Result<(), String> {
    let data_dir = cx
        .try_global::<SettingsStore>()
        .map(|store| store.data_dir.clone())
        .ok_or_else(|| SAVE_ERROR.to_string())?;
    let destination = prepared.0.as_ref().unwrap().clone();
    if destination.parent() != Some(data_dir.join(IMAGE_DIR).as_path()) {
        return Err("The data folder changed. Choose the image again.".into());
    }
    let mut next = current(cx);
    let previous = next
        .profile_images_by_account
        .insert(account_key, destination);
    next.save(&data_dir).map_err(|_| SAVE_ERROR.to_string())?;
    prepared.0.take();
    replace(next, SavePolicy::Immediate, cx);
    retire(previous, &data_dir, cx);
    cx.refresh_windows();
    Ok(())
}

pub(crate) fn remove(account_key: &str, cx: &mut App) -> Result<(), String> {
    let data_dir = cx
        .try_global::<SettingsStore>()
        .map(|store| store.data_dir.clone())
        .ok_or_else(|| SAVE_ERROR.to_string())?;
    let mut next = current(cx);
    let previous = next.profile_images_by_account.remove(account_key);
    if previous.is_none() {
        return Ok(());
    }
    next.save(&data_dir).map_err(|_| {
        "Unable to remove the profile image. Check folder permissions and try again.".to_string()
    })?;
    replace(next, SavePolicy::Immediate, cx);
    retire(previous, &data_dir, cx);
    cx.refresh_windows();
    Ok(())
}

fn retire(previous: Option<PathBuf>, data_dir: &Path, cx: &mut App) {
    let Some(path) = previous else {
        return;
    };
    // A hand-edited setting must never make Zeron delete the user's original file.
    let managed = path.parent() == Some(data_dir.join(IMAGE_DIR).as_path())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("profile-image-") && name.ends_with(".png"));
    let shared = current(cx)
        .profile_images_by_account
        .values()
        .any(|other| other == &path);
    if managed && !shared {
        let _ = std::fs::remove_file(&path);
        cx.defer(move |cx| gpui::ImageSource::from(path).evict(None, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{UiSettings, init};
    use super::*;

    fn source(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        image::RgbaImage::from_pixel(400, 200, image::Rgba([20, 100, 200, 255]))
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        path
    }

    #[test]
    fn keys_separate_accounts_and_local_mode_even_with_a_cached_user() {
        let user = UserProfile {
            id: "user-1".into(),
            email: "one@example.test".into(),
            name: None,
        };
        let other = UserProfile {
            id: "user-2".into(),
            ..user.clone()
        };
        assert_eq!(
            account_key(Some(WorkspaceScope::Local), Some(&user)).as_deref(),
            Some("local")
        );
        assert_eq!(
            account_key(Some(WorkspaceScope::Synced), Some(&user)).as_deref(),
            Some("user:user-1")
        );
        assert_ne!(
            account_key(Some(WorkspaceScope::Synced), Some(&user)),
            account_key(Some(WorkspaceScope::Synced), Some(&other))
        );
        assert_eq!(account_key(Some(WorkspaceScope::Synced), None), None);
        assert_eq!(account_key(None, None), None);
    }

    #[test]
    fn copied_image_is_a_static_square_png_and_uncommitted_copies_are_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let original = source(dir.path(), "photo.dat");
        let original_bytes = std::fs::read(&original).unwrap();
        let prepared = prepare(&original, dir.path()).unwrap();
        let path = prepared.0.as_ref().unwrap().clone();
        let decoded = image::open(&path).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (AVATAR_SIDE, AVATAR_SIDE)
        );
        assert_eq!(
            image::guess_format(&std::fs::read(&path).unwrap()).unwrap(),
            image::ImageFormat::Png
        );
        drop(prepared);
        assert!(!path.exists());
        assert_eq!(std::fs::read(&original).unwrap(), original_bytes);
    }

    #[test]
    fn invalid_and_oversized_images_are_rejected_without_creating_copies() {
        let dir = tempfile::tempdir().unwrap();
        let invalid = dir.path().join("broken.png");
        std::fs::write(&invalid, b"not an image").unwrap();
        assert!(prepare(&invalid, dir.path()).is_err());
        let oversized = dir.path().join("oversized.png");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        let error = prepare(&oversized, dir.path()).err().unwrap();
        assert!(error.contains("20 MB"));
        assert!(!dir.path().join(IMAGE_DIR).exists());
    }

    #[gpui::test]
    fn replacement_and_removal_persist_without_touching_other_accounts(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let original = source(dir.path(), "photo.png");
        cx.update(|cx| {
            init(UiSettings::default(), dir.path(), cx);
            install(
                "user:one".into(),
                prepare(&original, dir.path()).unwrap(),
                cx,
            )
            .unwrap();
            install(
                "user:two".into(),
                prepare(&original, dir.path()).unwrap(),
                cx,
            )
            .unwrap();
            let old = path("user:one", cx).unwrap();
            let other = path("user:two", cx).unwrap();
            install(
                "user:one".into(),
                prepare(&original, dir.path()).unwrap(),
                cx,
            )
            .unwrap();
            let replacement = path("user:one", cx).unwrap();
            assert_ne!(old, replacement);
            assert!(!old.exists());
            assert_eq!(UiSettings::load(dir.path()), current(cx));
            remove("user:one", cx).unwrap();
            assert!(path("user:one", cx).is_none());
            assert!(!replacement.exists());
            assert!(other.exists() && original.exists());
            assert_eq!(UiSettings::load(dir.path()), current(cx));
        });
    }

    #[gpui::test]
    fn failed_settings_write_keeps_the_previous_image_and_discards_the_candidate(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let original = source(dir.path(), "photo.png");
        cx.update(|cx| {
            init(UiSettings::default(), dir.path(), cx);
            install("local".into(), prepare(&original, dir.path()).unwrap(), cx).unwrap();
            let old = path("local", cx).unwrap();
            let prepared = prepare(&original, dir.path()).unwrap();
            let candidate = prepared.0.as_ref().unwrap().clone();
            let blocked = UiSettings::path(dir.path()).with_extension("json.tmp");
            std::fs::create_dir(&blocked).unwrap();
            assert!(install("local".into(), prepared, cx).is_err());
            assert_eq!(path("local", cx), Some(old.clone()));
            assert!(old.exists() && !candidate.exists());
            assert!(remove("local", cx).is_err());
            assert_eq!(path("local", cx), Some(old.clone()));
            assert!(old.exists());
        });
    }

    #[gpui::test]
    fn removing_a_hand_edited_path_never_deletes_the_source(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let original = source(dir.path(), "photo.png");
        cx.update(|cx| {
            let mut settings = UiSettings::default();
            settings
                .profile_images_by_account
                .insert("local".into(), original.clone());
            init(settings, dir.path(), cx);
            remove("local", cx).unwrap();
            assert!(original.exists());
        });
    }
}
