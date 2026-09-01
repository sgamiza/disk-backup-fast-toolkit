use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use walkdir::WalkDir;
use zip::write::{FileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipWriter};
use rayon::prelude::*;

const STATE_FILE_NAME: &str = ".backup_resume_state.json";
const SHOW_PROGRESS: bool = true;
// Parallel workers for top-level items. Use 1 to disable parallelism.
const MAX_WORKERS: usize = 6;
// Enable fastest mode: store without compression.
const FAST_MODE: bool = true;
// Buffer size used for file copies into zip.
const BUFFER_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct BackupResult {
    source: PathBuf,
    archive: PathBuf,
    ok: bool,
    message: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResumeState {
    stamp: String,
    done: Vec<String>,
    updated_at: String,
}

fn env_path(name: &str, default: PathBuf) -> PathBuf {
    env::var_os(name).map(PathBuf::from).unwrap_or(default)
}

fn env_flag(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no"
        ),
        Err(_) => default,
    }
}

fn system_drive_root() -> PathBuf {
    let drive = env::var_os("SystemDrive")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:"));
    drive.join("\\")
}

fn user_home() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(system_drive_root)
}

fn load_backup_config(config_path: &Path) -> io::Result<(Vec<String>, Vec<String>, Vec<String>)> {
    if !config_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Config file not found: {}", config_path.display()),
        ));
    }

    let content = fs::read_to_string(config_path)?;
    let mut items: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let mut includes: Vec<String> = Vec::new();
    let mut section: Option<&str> = None;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            continue;
        }

        if line == "backup_items:" {
            section = Some("items");
            continue;
        }
        if line == "exclude_items:" {
            section = Some("excludes");
            continue;
        }
        if line == "include_items:" {
            section = Some("includes");
            continue;
        }

        if section.is_none() {
            continue;
        }

        if !line.starts_with('-') {
            continue;
        }

        let mut value = line.trim_start_matches('-').trim().to_string();
        if value.is_empty() {
            continue;
        }

        let quoted = (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''));
        if !quoted {
            if let Some(pos) = value.find(" #") {
                value = value[..pos].trim().to_string();
            }
        }

        if quoted && value.len() >= 2 {
            value = value[1..value.len() - 1].to_string();
        }

        if value.is_empty() {
            continue;
        }

        match section {
            Some("items") => items.push(value),
            Some("excludes") => excludes.push(value),
            Some("includes") => includes.push(value),
            _ => {}
        }
    }

    if items.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Config has no valid backup items in 'backup_items' list",
        ));
    }

    Ok((items, excludes, includes))
}

fn normalize_path(path: &Path) -> String {
    let s = match path.canonicalize() {
        Ok(v) => v,
        Err(_) => path.to_path_buf(),
    };
    s.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn is_absolute_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        return true;
    }
    value.starts_with("\\\\")
}

fn resolve_item(item: &str, backup_root: &Path) -> PathBuf {
    if is_absolute_windows_path(item) {
        PathBuf::from(item)
    } else {
        backup_root.join(item)
    }
}

fn resolve_excludes(excludes: &[String], base_root: &Path) -> Vec<PathBuf> {
    let mut resolved = Vec::new();
    for value in excludes {
        if is_absolute_windows_path(value) {
            resolved.push(PathBuf::from(value));
        } else {
            resolved.push(base_root.join(value));
        }
    }
    resolved
}

fn resolve_includes(includes: &[String], base_root: &Path) -> Vec<PathBuf> {
    let mut resolved = Vec::new();
    for value in includes {
        if is_absolute_windows_path(value) {
            resolved.push(PathBuf::from(value));
        } else {
            resolved.push(base_root.join(value));
        }
    }
    resolved
}

fn is_excluded(path: &Path, exclude_norms: &[String], include_norms: &[String]) -> bool {
    let path_norm = normalize_path(path);
    for inc_norm in include_norms {
        if path_norm == *inc_norm || path_norm.starts_with(&(inc_norm.to_string() + "\\")) {
            return false;
        }
    }
    for ex_norm in exclude_norms {
        if path_norm == *ex_norm || path_norm.starts_with(&(ex_norm.to_string() + "\\")) {
            return true;
        }
    }
    false
}

fn is_ancestor_of_include(path: &Path, include_norms: &[String]) -> bool {
    let path_norm = normalize_path(path);
    for inc_norm in include_norms {
        if *inc_norm == path_norm || inc_norm.starts_with(&(path_norm.to_string() + "\\")) {
            return true;
        }
    }
    false
}

fn ensure_output_dir(path: &Path) -> io::Result<()> {
    if path.exists() && path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Output path exists and is a file: {}", path.display()),
        ));
    }
    fs::create_dir_all(path)
}

fn make_archive_name(source: &Path, stamp: &str, all_sources: &[PathBuf]) -> String {
    let base_name = source
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("root");

    // Check if another source shares the same base name
    let duplicates = all_sources
        .iter()
        .filter(|s| {
            s.file_name().and_then(|v| v.to_str()) == Some(base_name)
        })
        .count();

    if duplicates > 1 {
        // Use parent dir name as prefix to disambiguate
        let parent = source
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|v| v.to_str())
            .unwrap_or("unknown");
        format!("{}_{}_{}.zip", parent, base_name, stamp)
    } else {
        format!("{}_{}.zip", base_name, stamp)
    }
}

fn log_warn(path: Option<&Path>, message: impl AsRef<str>) {
    if let Some(p) = path {
        println!("[WARN] {} | reason: {}", p.display(), message.as_ref());
    } else {
        println!("[WARN] {}", message.as_ref());
    }
}

fn zip_single_file(file_path: &Path, archive_path: &Path) -> io::Result<()> {
    let file = File::create(archive_path)?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let options: SimpleFileOptions = if FAST_MODE {
        FileOptions::default().compression_method(CompressionMethod::Stored)
    } else {
        FileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(1))
    };

    let mut src = BufReader::with_capacity(BUFFER_SIZE, File::open(file_path)?);
    zip.start_file(
        file_path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("file.bin"),
        options,
    )?;
    io::copy(&mut src, &mut zip)?;
    zip.finish()?;
    Ok(())
}

fn zip_folder(
    folder_path: &Path,
    archive_path: &Path,
    exclude_norms: &[String],
    include_norms: &[String],
) -> io::Result<()> {
    let file = File::create(archive_path)?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let options: SimpleFileOptions = if FAST_MODE {
        FileOptions::default().compression_method(CompressionMethod::Stored)
    } else {
        FileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(1))
    };

    let parent = folder_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Folder has no parent"))?;

    let exclude_norms = exclude_norms.to_vec();
    let include_norms = include_norms.to_vec();

    let walker = WalkDir::new(folder_path).into_iter().filter_entry({
        let exclude_norms = exclude_norms.clone();
        let include_norms = include_norms.clone();
        move |e| {
            let p = e.path();
            !is_excluded(p, &exclude_norms, &include_norms)
                || is_ancestor_of_include(p, &include_norms)
        }
    });

    for entry in walker {
        let entry = match entry {
            Ok(v) => v,
            Err(e) => {
                log_warn(e.path(), e.to_string());
                continue;
            }
        };

        let p = entry.path();

        if is_excluded(p, &exclude_norms, &include_norms)
            && !is_ancestor_of_include(p, &include_norms)
        {
            continue;
        }

        let rel = match p.strip_prefix(parent) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if entry.file_type().is_dir() {
            match fs::read_dir(p) {
                Ok(mut it) => {
                    if it.next().is_none() {
                        if let Err(e) = zip.add_directory(format!("{}/", rel_str), options) {
                            log_warn(Some(p), e.to_string());
                        }
                    }
                }
                Err(e) => log_warn(Some(p), e.to_string()),
            }
            continue;
        }

        if entry.file_type().is_file() {
            let src = match File::open(p) {
                Ok(v) => v,
                Err(e) => {
                    log_warn(Some(p), e.to_string());
                    continue;
                }
            };
            let mut src = BufReader::with_capacity(BUFFER_SIZE, src);

            if let Err(e) = zip.start_file(rel_str, options) {
                log_warn(Some(p), e.to_string());
                continue;
            }

            if let Err(e) = io::copy(&mut src, &mut zip) {
                log_warn(Some(p), e.to_string());
                continue;
            }
        }
    }

    zip.finish()?;
    Ok(())
}

fn backup_one(
    source: &Path,
    output_dir: &Path,
    stamp: &str,
    exclude_norms: &[String],
    include_norms: &[String],
    dry_run: bool,
    all_sources: &[PathBuf],
) -> BackupResult {
    if !source.exists() {
        return BackupResult {
            source: source.to_path_buf(),
            archive: PathBuf::new(),
            ok: false,
            message: "Path not found".to_string(),
        };
    }

    if is_excluded(source, exclude_norms, include_norms) {
        return BackupResult {
            source: source.to_path_buf(),
            archive: PathBuf::new(),
            ok: true,
            message: "Skip excluded path".to_string(),
        };
    }

    let archive_path = output_dir.join(make_archive_name(source, stamp, all_sources));

    if dry_run {
        if source.is_file() || source.is_dir() {
            return BackupResult {
                source: source.to_path_buf(),
                archive: archive_path,
                ok: true,
                message: "DRY RUN: not created".to_string(),
            };
        }
        return BackupResult {
            source: source.to_path_buf(),
            archive: archive_path,
            ok: false,
            message: "Unsupported path type".to_string(),
        };
    }

    let result = if source.is_file() {
        zip_single_file(source, &archive_path)
    } else if source.is_dir() {
        zip_folder(source, &archive_path, exclude_norms, include_norms)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Unsupported path type",
        ))
    };

    match result {
        Ok(_) => BackupResult {
            source: source.to_path_buf(),
            archive: archive_path,
            ok: true,
            message: String::new(),
        },
        Err(e) => BackupResult {
            source: source.to_path_buf(),
            archive: PathBuf::new(),
            ok: false,
            message: e.to_string(),
        },
    }
}

fn get_state_file(output_dir: &Path) -> PathBuf {
    output_dir.join(STATE_FILE_NAME)
}

fn save_state(output_dir: &Path, stamp: &str, done_items: &HashSet<String>) -> io::Result<()> {
    let state_file = get_state_file(output_dir);
    let mut done: Vec<String> = done_items.iter().cloned().collect();
    done.sort();
    let state = ResumeState {
        stamp: stamp.to_string(),
        done,
        updated_at: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
    };
    let content = serde_json::to_string_pretty(&state)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(state_file, content)
}

fn load_or_create_state(output_dir: &Path) -> io::Result<(String, HashSet<String>)> {
    let state_file = get_state_file(output_dir);
    if state_file.exists() {
        let content = fs::read_to_string(&state_file)?;
        let state: ResumeState = serde_json::from_str(&content)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        return Ok((state.stamp, state.done.into_iter().collect()));
    }

    let stamp = Local::now().format("%Y%m%d").to_string();
    save_state(output_dir, &stamp, &HashSet::new())?;
    Ok((stamp, HashSet::new()))
}

fn clear_state(output_dir: &Path) {
    let state_file = get_state_file(output_dir);
    if state_file.exists() {
        let _ = fs::remove_file(state_file);
    }
}

fn render_progress(current: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return "[----------------------------] 0/0 (0%)".to_string();
    }
    let ratio = current as f64 / total as f64;
    let filled = (ratio * width as f64) as usize;
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(width - filled));
    let percent = (ratio * 100.0) as usize;
    format!("[{}] {}/{} ({}%)", bar, current, total, percent)
}

fn main() -> io::Result<()> {
    let start_clock = Instant::now();
    let start_time = Local::now();

    let backup_root = env_path("BACKUP_ROOT", system_drive_root());
    let config_path = env_path(
        "BACKUP_CONFIG",
        PathBuf::from("..").join("backup_items.yaml"),
    );
    let output_dir = env_path("BACKUP_OUTPUT_DIR", PathBuf::from(r"C:\backup_rust_v2"));
    let exclude_base = env_path("BACKUP_EXCLUDE_BASE", user_home());
    let dry_run = env_flag("BACKUP_DRY_RUN", false);

    let (backup_items, exclude_items, include_items) = load_backup_config(&config_path)?;
    ensure_output_dir(&output_dir)?;

    let exclude_paths = resolve_excludes(&exclude_items, &exclude_base);
    let include_paths = resolve_includes(&include_items, &exclude_base);
    let mut exclude_norms = Vec::new();
    exclude_norms.push(normalize_path(&output_dir));
    for p in exclude_paths {
        exclude_norms.push(normalize_path(&p));
    }
    let mut include_norms = Vec::new();
    for p in include_paths {
        include_norms.push(normalize_path(&p));
    }

    let (stamp, mut done_items): (String, HashSet<String>) = if dry_run {
        (Local::now().format("%Y%m%d").to_string(), HashSet::new())
    } else {
        load_or_create_state(&output_dir)?
    };

    println!("Mode: {}", if dry_run { "DRY RUN" } else { "REAL RUN" });
    println!("Output dir: {}", output_dir.display());
    println!("Config: {}", config_path.display());
    println!("Backup root: {}", backup_root.display());
    println!("Exclude base: {}", exclude_base.display());
    println!("Start time: {}", start_time.format("%Y-%m-%d %H:%M:%S"));

    let total = backup_items.len();
    let mut results: Vec<BackupResult> = Vec::with_capacity(total);
    let mut pending_sources: Vec<PathBuf> = Vec::new();
    let all_sources: Vec<PathBuf> = backup_items
        .iter()
        .map(|item| resolve_item(item, &backup_root))
        .collect();

    for (idx, item) in backup_items.iter().enumerate() {
        let source = resolve_item(item, &backup_root);

        if SHOW_PROGRESS {
            let p = render_progress(idx, total, 28);
            print!("\r{} | Processing: {}    ", p, source.display());
            let _ = io::stdout().flush();
        }

        if dry_run {
            results.push(backup_one(
                &source,
                &output_dir,
                &stamp,
                &exclude_norms,
                &include_norms,
                true,
                &all_sources,
            ));
            continue;
        }

        let source_key = normalize_path(&source);
        let archive = output_dir.join(make_archive_name(&source, &stamp, &all_sources));

        if done_items.contains(&source_key) {
            results.push(BackupResult {
                source,
                archive,
                ok: true,
                message: "Already done (resume)".to_string(),
            });
            continue;
        }

        if archive.exists() {
            done_items.insert(source_key.clone());
            let _ = save_state(&output_dir, &stamp, &done_items);
            results.push(BackupResult {
                source,
                archive,
                ok: true,
                message: "Archive exists, skipped".to_string(),
            });
            continue;
        }

        pending_sources.push(source);
    }

    if !dry_run && !pending_sources.is_empty() {
        if MAX_WORKERS > 1 {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(MAX_WORKERS)
                .build()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            let mut batch_results: Vec<BackupResult> = pool.install(|| {
                pending_sources
                    .par_iter()
                    .map(|source| {
                        backup_one(
                            source,
                            &output_dir,
                            &stamp,
                            &exclude_norms,
                            &include_norms,
                            false,
                            &all_sources,
                        )
                    })
                    .collect()
            });

            results.append(&mut batch_results);
        } else {
            for source in pending_sources {
                let res = backup_one(
                    &source,
                    &output_dir,
                    &stamp,
                    &exclude_norms,
                    &include_norms,
                    false,
                    &all_sources,
                );
                results.push(res);
            }
        }

        for r in &results {
            if r.ok {
                let source_key = normalize_path(&r.source);
                done_items.insert(source_key);
                let _ = save_state(&output_dir, &stamp, &done_items);
            }
        }
    }

    if SHOW_PROGRESS {
        let p = render_progress(total, total, 28);
        println!("\r{} | Completed                                ", p);
    }

    let ok_count = results.iter().filter(|r| r.ok).count();
    let fail_count = results.len().saturating_sub(ok_count);

    if !dry_run && fail_count == 0 {
        clear_state(&output_dir);
    }

    println!("Backup finished. success={}, failed={}", ok_count, fail_count);
    for r in results {
        if r.ok {
            if r.message.is_empty() {
                println!("[OK] {} -> {}", r.source.display(), r.archive.display());
            } else {
                println!(
                    "[OK] {} -> {} ({})",
                    r.source.display(),
                    r.archive.display(),
                    r.message
                );
            }
        } else {
            println!("[FAIL] {} | reason: {}", r.source.display(), r.message);
        }
    }

    let end_time = Local::now();
    let elapsed_secs = start_clock.elapsed().as_secs_f64();
    println!("End time: {}", end_time.format("%Y-%m-%d %H:%M:%S"));
    println!("Elapsed seconds: {:.2}", elapsed_secs);

    Ok(())
}
