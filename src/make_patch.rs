use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::Path;

use anyhow::Context;
use flate2::write::GzEncoder;
use qbsdiff::Bsdiff;
use tar::Builder as TarBuilder;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::extract::{extract_tar_gz, extract_zip, relative_path_string, sanitise_path};
use crate::hash::{hash_bytes, hash_directory, hash_file};
use crate::patch::{PatchFileEntry, PatchIndex, PatchOp, bsdiff_entry_name, create_entry_name};
use crate::platform::{LINUX_PLATFORM, WINDOWS_PLATFORM, game_archive_asset_name};
use crate::version::VERSION_FILE_NAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchPlatform {
    Windows,
    Linux,
    Both,
}

impl std::str::FromStr for PatchPlatform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "windows" | "win" => Ok(Self::Windows),
            "linux" => Ok(Self::Linux),
            "both" | "all" => Ok(Self::Both),
            _ => Err(format!(
                "unknown platform '{s}', expected 'windows', 'linux', or 'both'"
            )),
        }
    }
}

impl PatchPlatform {
    fn include_windows(self) -> bool {
        matches!(self, Self::Windows | Self::Both)
    }

    fn include_linux(self) -> bool {
        matches!(self, Self::Linux | Self::Both)
    }
}

pub fn make_patch(
    old_dir: impl AsRef<Path>,
    new_dir: impl AsRef<Path>,
    from_tag: &str,
    to_tag: &str,
    out_dir: impl AsRef<Path>,
    platform: PatchPlatform,
) -> anyhow::Result<()> {
    let old_dir = old_dir.as_ref();
    let new_dir = new_dir.as_ref();
    let out_dir = out_dir.as_ref();

    anyhow::ensure!(
        old_dir.is_dir(),
        "old-dir is not a directory: {}",
        old_dir.display()
    );
    anyhow::ensure!(
        new_dir.is_dir(),
        "new-dir is not a directory: {}",
        new_dir.display()
    );
    validate_game_tag(from_tag, "from-tag")?;
    validate_game_tag(to_tag, "to-tag")?;
    anyhow::ensure!(from_tag != to_tag, "from-tag and to-tag must differ");
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create out-dir {}", out_dir.display()))?;

    let old_files = collect_files(old_dir)?;
    let new_files = collect_files(new_dir)?;

    let windows_name = format!("{from_tag}-to-{to_tag}.patch");
    let linux_name = format!("{from_tag}-to-{to_tag}-linux.patch");

    let common = PatchBundleInputs {
        old_dir,
        new_dir,
        from_tag,
        to_tag,
        old_files: &old_files,
        new_files: &new_files,
    };

    if platform.include_windows() {
        generate_patch_bundle(PatchBundleJob {
            inputs: common,
            output_path: out_dir.join(windows_name),
            include: include_in_windows_bundle,
        })?;
    }
    if platform.include_linux() {
        generate_patch_bundle(PatchBundleJob {
            inputs: common,
            output_path: out_dir.join(linux_name),
            include: include_in_linux_bundle,
        })?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
struct PatchBundleInputs<'a> {
    old_dir: &'a Path,
    new_dir: &'a Path,
    from_tag: &'a str,
    to_tag: &'a str,
    old_files: &'a BTreeSet<String>,
    new_files: &'a BTreeSet<String>,
}

struct PatchBundleJob<'a> {
    inputs: PatchBundleInputs<'a>,
    output_path: std::path::PathBuf,
    include: fn(&str) -> bool,
}

fn generate_patch_bundle(job: PatchBundleJob<'_>) -> anyhow::Result<()> {
    let mut union = BTreeSet::new();
    for path in job
        .inputs
        .old_files
        .iter()
        .chain(job.inputs.new_files.iter())
    {
        if (job.include)(path) {
            union.insert(path.clone());
        }
    }

    let mut index = PatchIndex {
        from: job.inputs.from_tag.to_string(),
        to: job.inputs.to_tag.to_string(),
        files: BTreeMap::new(),
    };
    let mut stats = PatchStats::default();
    let mut payloads: Vec<(String, Vec<u8>)> = Vec::new();

    for relative_path in union {
        sanitise_path(&relative_path)?
            .ok_or_else(|| anyhow::anyhow!("invalid relative path {relative_path}"))?;

        let old_path = job.inputs.old_dir.join(&relative_path);
        let new_path = job.inputs.new_dir.join(&relative_path);
        let exists_old = old_path.exists();
        let exists_new = new_path.exists();

        match (exists_old, exists_new) {
            (true, true) => {
                let old_data = std::fs::read(&old_path)
                    .with_context(|| format!("failed to read {}", old_path.display()))?;
                let new_data = std::fs::read(&new_path)
                    .with_context(|| format!("failed to read {}", new_path.display()))?;
                let old_hash = hash_bytes(&old_data);
                let new_hash = hash_bytes(&new_data);

                if old_data == new_data {
                    stats.unchanged += 1;
                    index.files.insert(
                        relative_path,
                        PatchFileEntry {
                            op: PatchOp::Unchanged,
                            bsdiff: None,
                            hash_before: None,
                            hash_after: None,
                            hash: Some(old_hash),
                        },
                    );
                } else {
                    stats.updated += 1;
                    let mut patch_data = Vec::new();
                    Bsdiff::new(&old_data, &new_data)
                        .compare(Cursor::new(&mut patch_data))
                        .with_context(|| {
                            format!("failed to generate bsdiff for {relative_path}")
                        })?;

                    let patch_entry_name = bsdiff_entry_name(&relative_path);
                    payloads.push((patch_entry_name.clone(), patch_data));
                    index.files.insert(
                        relative_path,
                        PatchFileEntry {
                            op: PatchOp::Update,
                            bsdiff: Some(patch_entry_name),
                            hash_before: Some(old_hash),
                            hash_after: Some(new_hash),
                            hash: None,
                        },
                    );
                }
            }
            (false, true) => {
                stats.created += 1;
                let data = std::fs::read(&new_path)
                    .with_context(|| format!("failed to read {}", new_path.display()))?;
                let hash = hash_bytes(&data);
                let create_entry = create_entry_name(&relative_path);
                payloads.push((create_entry, data));
                index.files.insert(
                    relative_path,
                    PatchFileEntry {
                        op: PatchOp::Create,
                        bsdiff: None,
                        hash_before: None,
                        hash_after: None,
                        hash: Some(hash),
                    },
                );
            }
            (true, false) => {
                stats.deleted += 1;
                let old_hash = hash_file(&old_path)?;
                index.files.insert(
                    relative_path,
                    PatchFileEntry {
                        op: PatchOp::Delete,
                        bsdiff: None,
                        hash_before: Some(old_hash),
                        hash_after: None,
                        hash: None,
                    },
                );
            }
            (false, false) => {
                anyhow::bail!("path {relative_path} exists in neither old nor new dir")
            }
        }
    }

    anyhow::ensure!(
        stats.total() > 0,
        "refusing to write empty patch bundle {}",
        job.output_path.display()
    );

    write_patch_zip(&job.output_path, &index, payloads)?;
    println!(
        "Wrote {} ({} files: {} updated, {} created, {} deleted, {} unchanged)",
        job.output_path.display(),
        stats.total(),
        stats.updated,
        stats.created,
        stats.deleted,
        stats.unchanged
    );
    Ok(())
}

#[derive(Default)]
struct PatchStats {
    updated: usize,
    created: usize,
    deleted: usize,
    unchanged: usize,
}

impl PatchStats {
    fn total(&self) -> usize {
        self.updated + self.created + self.deleted + self.unchanged
    }
}

fn create_zip_writer(output_path: &Path) -> anyhow::Result<(ZipWriter<File>, SimpleFileOptions)> {
    let file = File::create(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    let zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    Ok((zip, options))
}

fn write_patch_zip(
    output_path: &Path,
    index: &PatchIndex,
    payloads: Vec<(String, Vec<u8>)>,
) -> anyhow::Result<()> {
    let (mut zip, options) = create_zip_writer(output_path)?;

    zip.start_file("patch.index", options)
        .context("failed to write patch.index ZIP entry")?;
    let index_json = serde_json::to_vec_pretty(index).context("failed to serialize patch.index")?;
    zip.write_all(&index_json)
        .context("failed to write patch.index")?;

    for (name, data) in payloads {
        sanitise_path(&name)?.ok_or_else(|| anyhow::anyhow!("invalid ZIP payload path {name}"))?;
        zip.start_file(&name, options)
            .with_context(|| format!("failed to write ZIP entry {name}"))?;
        zip.write_all(&data)
            .with_context(|| format!("failed to write ZIP entry {name}"))?;
    }

    zip.finish().context("failed to finish patch ZIP")?;
    Ok(())
}

fn collect_files(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();

    for entry in WalkDir::new(root) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry.path().strip_prefix(root).with_context(|| {
            format!(
                "failed to make {} relative to {}",
                entry.path().display(),
                root.display()
            )
        })?;
        let relative_text = relative_path_string(relative);
        if should_skip_dev_file(&relative_text) {
            continue;
        }
        files.insert(relative_text);
    }

    Ok(files)
}

fn should_skip_dev_file(relative_path: &str) -> bool {
    relative_path == VERSION_FILE_NAME
        || relative_path
            .split('/')
            .any(|component| component == "__MACOSX")
        || relative_path
            .split('/')
            .any(|component| component == ".DS_Store")
}

fn include_in_windows_bundle(relative_path: &str) -> bool {
    let lower = relative_path.to_ascii_lowercase();
    !has_path_component(&lower, "linux") && !lower.ends_with(".x86_64") && !lower.ends_with(".so")
}

fn include_in_linux_bundle(relative_path: &str) -> bool {
    let lower = relative_path.to_ascii_lowercase();
    !has_path_component(&lower, "win")
        && !lower.ends_with(".exe")
        && !lower.ends_with(".dll")
        && !lower.ends_with(".pdb")
        && !lower.ends_with(".manifest")
        && !lower.ends_with(".lib")
        && !lower.ends_with(".exp")
}

fn has_path_component(relative_path: &str, component: &str) -> bool {
    relative_path.split('/').any(|part| part == component)
        || relative_path.split('\\').any(|part| part == component)
}

fn validate_game_tag(tag: &str, name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !tag.is_empty()
            && !tag.starts_with('v')
            && tag.as_bytes()[0].is_ascii_digit()
            && tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "invalid {name} {tag:?}; use a tag such as 0.4 without a leading v"
    );
    Ok(())
}

pub fn make_archive(
    dir: impl AsRef<Path>,
    version: &str,
    out_dir: impl AsRef<Path>,
    platform: Option<PatchPlatform>,
) -> anyhow::Result<()> {
    let dir = dir.as_ref();
    let out_dir = out_dir.as_ref();

    anyhow::ensure!(dir.is_dir(), "dir is not a directory: {}", dir.display());
    validate_game_tag(version, "version")?;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create out-dir {}", out_dir.display()))?;

    let all_files = collect_files(dir)?;
    anyhow::ensure!(!all_files.is_empty(), "no files found in {}", dir.display());

    let build_windows = platform.map_or_else(
        || cfg!(windows),
        |p| p == PatchPlatform::Windows || p == PatchPlatform::Both,
    );
    let build_linux = platform.map_or_else(
        || !cfg!(windows),
        |p| p == PatchPlatform::Linux || p == PatchPlatform::Both,
    );

    if build_windows {
        let windows_files = filtered_files(&all_files, include_in_windows_bundle);
        let output_path = out_dir.join(game_archive_asset_name(WINDOWS_PLATFORM, version)?);
        finish_archive(dir, &windows_files, &output_path, create_zip_archive)?;
    }

    if build_linux {
        let linux_files = filtered_files(&all_files, include_in_linux_bundle);
        let output_path = out_dir.join(game_archive_asset_name(LINUX_PLATFORM, version)?);
        finish_archive(dir, &linux_files, &output_path, create_tar_gz_archive)?;
    }

    Ok(())
}

fn finish_archive(
    root: &Path,
    files: &BTreeSet<String>,
    output_path: &Path,
    create_archive: fn(&Path, &BTreeSet<String>, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !files.is_empty(),
        "refusing to create an empty archive {}",
        output_path.display()
    );
    create_archive(root, files, output_path)?;
    println!("Wrote {}", output_path.display());
    write_content_hash(output_path, &archive_content_hash(output_path)?)
}

fn filtered_files(files: &BTreeSet<String>, include: impl Fn(&str) -> bool) -> BTreeSet<String> {
    files.iter().filter(|path| include(path)).cloned().collect()
}

fn create_zip_archive(
    root: &Path,
    files: &BTreeSet<String>,
    output_path: &Path,
) -> anyhow::Result<()> {
    let (mut zip, options) = create_zip_writer(output_path)?;

    for relative_path in files {
        sanitise_path(relative_path)?
            .ok_or_else(|| anyhow::anyhow!("invalid archive path {relative_path}"))?;

        let source_path = root.join(relative_path);
        zip.start_file(relative_path, options)
            .with_context(|| format!("failed to write ZIP entry {relative_path}"))?;
        let data = std::fs::read(&source_path)
            .with_context(|| format!("failed to read {}", source_path.display()))?;
        zip.write_all(&data)
            .with_context(|| format!("failed to write ZIP entry {relative_path}"))?;
    }

    zip.finish().context("failed to finish archive ZIP")?;
    Ok(())
}

fn create_tar_gz_archive(
    root: &Path,
    files: &BTreeSet<String>,
    output_path: &Path,
) -> anyhow::Result<()> {
    let file = File::create(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    let encoder = GzEncoder::new(file, flate2::Compression::default());
    let mut tar = TarBuilder::new(encoder);

    for relative_path in files {
        sanitise_path(relative_path)?
            .ok_or_else(|| anyhow::anyhow!("invalid archive path {relative_path}"))?;

        let source_path = root.join(relative_path);
        tar.append_path_with_name(&source_path, relative_path)
            .with_context(|| format!("failed to add {relative_path} to tar"))?;
    }

    let encoder = tar.into_inner().context("failed to finish tar archive")?;
    encoder
        .finish()
        .context("failed to finish gz compression")?;
    Ok(())
}

fn write_content_hash(archive_path: &Path, content_hash: &str) -> anyhow::Result<()> {
    let archive_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid archive path {}", archive_path.display()))?;
    let sidecar_path = archive_path.with_file_name(format!("{archive_name}.content-hash"));
    std::fs::write(&sidecar_path, format!("{content_hash}\n"))
        .with_context(|| format!("failed to write {}", sidecar_path.display()))?;
    println!("Wrote {}", sidecar_path.display());
    Ok(())
}

fn archive_content_hash(archive_path: &Path) -> anyhow::Result<String> {
    let temp_dir =
        tempfile::tempdir().context("failed to create archive verification directory")?;
    if archive_path
        .extension()
        .is_some_and(|extension| extension == "zip")
    {
        extract_zip(archive_path, temp_dir.path())?;
    } else {
        extract_tar_gz(archive_path, temp_dir.path())?;
    }
    hash_directory(temp_dir.path())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::extract::{extract_tar_gz, extract_zip};
    use crate::hash::hash_directory;
    use crate::platform::{LINUX_PLATFORM, WINDOWS_PLATFORM, game_archive_asset_name};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn archive_sidecars_match_extracted_content() {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "craftmoon-archive-test-{}-{sequence}",
            std::process::id()
        ));
        let source = root.join("source");
        let assets = root.join("assets");
        let linux_extracted = root.join("linux-extracted");
        let windows_extracted = root.join("windows-extracted");
        std::fs::create_dir_all(source.join("data")).unwrap();
        std::fs::write(source.join("CraftMoon-linux.x86_64"), b"game").unwrap();
        std::fs::write(source.join("data/content.bin"), [0, 1, 2, 3]).unwrap();
        std::fs::write(source.join("CraftMoon.exe"), b"windows game").unwrap();

        make_archive(&source, "0.5", &assets, Some(PatchPlatform::Both)).unwrap();
        let linux_archive = game_archive_asset_name(LINUX_PLATFORM, "0.5").unwrap();
        let windows_archive = game_archive_asset_name(WINDOWS_PLATFORM, "0.5").unwrap();
        extract_tar_gz(assets.join(&linux_archive), &linux_extracted).unwrap();
        extract_zip(assets.join(&windows_archive), &windows_extracted).unwrap();

        let linux_sidecar =
            std::fs::read_to_string(assets.join(format!("{linux_archive}.content-hash"))).unwrap();
        let windows_sidecar =
            std::fs::read_to_string(assets.join(format!("{windows_archive}.content-hash")))
                .unwrap();
        assert_eq!(
            linux_sidecar.trim(),
            hash_directory(&linux_extracted).unwrap()
        );
        assert_eq!(
            windows_sidecar.trim(),
            hash_directory(&windows_extracted).unwrap()
        );
        assert!(!linux_extracted.join("CraftMoon.exe").exists());
        assert!(!windows_extracted.join("CraftMoon-linux.x86_64").exists());

        std::fs::remove_dir_all(root).unwrap();
    }
}
