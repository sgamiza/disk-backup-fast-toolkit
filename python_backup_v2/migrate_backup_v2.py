from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from concurrent.futures import ProcessPoolExecutor, as_completed
import json
from pathlib import Path
import os
import shutil
import sys
import zipfile


PROJECT_ROOT = Path(__file__).resolve().parents[1]
CONFIG_PATH = Path(os.getenv("BACKUP_CONFIG", str(PROJECT_ROOT / "backup_items.yaml")))
SYSTEM_DRIVE = os.getenv("SystemDrive", "C:")
C_ROOT = Path(os.getenv("BACKUP_ROOT", SYSTEM_DRIVE + os.sep))
DEFAULT_EXCLUDE_BASE = Path(os.getenv("BACKUP_EXCLUDE_BASE", str(Path.home())))
OUTPUT_DIR = Path(
    os.getenv("BACKUP_OUTPUT_DIR", str(Path.home() / "backup_python_v2"))
)
TIME_FORMAT = "%Y%m%d"
STATE_FILE_NAME = ".backup_resume_state.json"
# Safe by default. Set BACKUP_DRY_RUN=0 only after reviewing the preview.
DRY_RUN = os.getenv("BACKUP_DRY_RUN", "1").strip().lower() not in {
    "0",
    "false",
    "no",
}
SHOW_PROGRESS = True
# Lower compression level is much faster with slightly larger zip files.
ZIP_COMPRESSLEVEL = 1
# Parallel workers for top-level items. Use 1 to disable parallelism.
MAX_WORKERS = 4
# Enable fastest mode: store without compression.
FAST_MODE = True
# Buffer size used for file copies into zip.
BUFFER_SIZE = 8 * 1024 * 1024


@dataclass
class BackupResult:
    source: Path
    archive: Path
    ok: bool
    message: str = ""


def load_backup_config(config_path: Path) -> tuple[list[str], list[str], list[str]]:
    """Load backup items, exclude items, and include items from a lightweight YAML file."""
    if not config_path.exists():
        raise RuntimeError(f"Config file not found: {config_path}")

    items: list[str] = []
    excludes: list[str] = []
    includes: list[str] = []
    section: str | None = None

    with config_path.open("r", encoding="utf-8") as f:
        for raw in f:
            line = raw.strip()
            if not line:
                continue
            if line.startswith("#"):
                continue

            if line == "backup_items:":
                section = "items"
                continue
            if line == "exclude_items:":
                section = "excludes"
                continue
            if line == "include_items:":
                section = "includes"
                continue

            if section is None:
                continue

            if not line.startswith("-"):
                continue

            value = line[1:].strip()
            if not value:
                continue

            # Remove inline comments for unquoted scalar values.
            if not ((value.startswith('"') and value.endswith('"')) or (value.startswith("'") and value.endswith("'"))):
                value = value.split(" #", 1)[0].strip()

            if (value.startswith('"') and value.endswith('"')) or (value.startswith("'") and value.endswith("'")):
                value = value[1:-1]

            if not value:
                continue

            if section == "items":
                items.append(value)
            elif section == "excludes":
                excludes.append(value)
            elif section == "includes":
                includes.append(value)

    if not items:
        raise RuntimeError("Config has no valid backup items in 'backup_items' list")

    return items, excludes, includes


def normalize_path(path: Path) -> str:
    """Return a case-insensitive normalized path string for comparisons."""
    return str(path.resolve(strict=False)).replace("/", "\\").lower()


def is_absolute_windows_path(value: str) -> bool:
    """Detect drive-qualified or UNC Windows paths."""
    p = Path(value)
    return p.is_absolute()


def resolve_item(item: str) -> Path:
    """Resolve list items; relative names use BACKUP_ROOT."""
    if is_absolute_windows_path(item):
        return Path(item)
    return C_ROOT / item


def resolve_excludes(excludes: list[str], base_root: Path) -> list[Path]:
    resolved: list[Path] = []
    for value in excludes:
        if is_absolute_windows_path(value):
            resolved.append(Path(value))
        else:
            resolved.append(base_root / value)
    return resolved


def resolve_includes(includes: list[str], base_root: Path) -> list[Path]:
    resolved: list[Path] = []
    for value in includes:
        if is_absolute_windows_path(value):
            resolved.append(Path(value))
        else:
            resolved.append(base_root / value)
    return resolved


def is_excluded(path: Path, exclude_norms: list[str], include_norms: list[str]) -> bool:
    path_norm = normalize_path(path)
    for inc_norm in include_norms:
        if path_norm == inc_norm or path_norm.startswith(inc_norm + "\\"):
            return False
    for ex_norm in exclude_norms:
        if path_norm == ex_norm or path_norm.startswith(ex_norm + "\\"):
            return True
    return False


def is_ancestor_of_include(path: Path, include_norms: list[str]) -> bool:
    path_norm = normalize_path(path)
    for inc_norm in include_norms:
        if inc_norm == path_norm or inc_norm.startswith(path_norm + "\\"):
            return True
    return False


def ensure_output_dir(path: Path) -> None:
    if path.exists() and path.is_file():
        raise RuntimeError(f"Output path exists and is a file: {path}")
    path.mkdir(parents=True, exist_ok=True)


def make_archive_name(source: Path, stamp: str) -> str:
    # For root-like sources, fallback to safe name.
    base_name = source.name if source.name else "root"
    return f"{base_name}_{stamp}.zip"


def zip_kwargs() -> dict:
    if FAST_MODE:
        return {"compression": zipfile.ZIP_STORED}
    return {"compression": zipfile.ZIP_DEFLATED, "compresslevel": ZIP_COMPRESSLEVEL}


def write_file_to_zip(zf: zipfile.ZipFile, full_path: Path, arcname: str) -> None:
    with full_path.open("rb") as src, zf.open(arcname, "w") as dest:
        shutil.copyfileobj(src, dest, length=BUFFER_SIZE)


def zip_single_file(file_path: Path, archive_path: Path) -> None:
    with zipfile.ZipFile(archive_path, mode="w", **zip_kwargs()) as zf:
        write_file_to_zip(zf, file_path, file_path.name)


def zip_folder(
    folder_path: Path,
    archive_path: Path,
    exclude_norms: list[str],
    include_norms: list[str],
) -> None:
    parent = folder_path.parent

    with zipfile.ZipFile(archive_path, mode="w", **zip_kwargs()) as zf:
        for root, dirs, files in os.walk(folder_path):
            root_path = Path(root)

            # Skip excluded directories to avoid walking them.
            kept_dirs: list[str] = []
            for d in dirs:
                candidate = root_path / d
                if not is_excluded(candidate, exclude_norms, include_norms):
                    kept_dirs.append(d)
                elif is_ancestor_of_include(candidate, include_norms):
                    kept_dirs.append(d)
            dirs[:] = kept_dirs

            if is_excluded(root_path, exclude_norms, include_norms) and not is_ancestor_of_include(
                root_path, include_norms
            ):
                continue

            relative_root = root_path.relative_to(parent)

            # Keep empty folders in archive.
            if not files and not dirs:
                zf.writestr(str(relative_root).replace("\\", "/") + "/", "")

            for file_name in files:
                full_path = root_path / file_name
                if is_excluded(full_path, exclude_norms, include_norms):
                    continue
                arcname = full_path.relative_to(parent)
                write_file_to_zip(zf, full_path, str(arcname).replace("\\", "/"))


def backup_one(
    source: Path,
    output_dir: Path,
    stamp: str,
    exclude_norms: list[str],
    include_norms: list[str],
    dry_run: bool = False,
) -> BackupResult:
    try:
        if not source.exists():
            return BackupResult(source=source, archive=Path(), ok=False, message="Path not found")

        if is_excluded(source, exclude_norms, include_norms):
            return BackupResult(source=source, archive=Path(), ok=True, message="Skip excluded path")

        archive_path = output_dir / make_archive_name(source, stamp)

        if dry_run:
            if source.is_file() or source.is_dir():
                return BackupResult(source=source, archive=archive_path, ok=True, message="DRY RUN: not created")
            return BackupResult(source=source, archive=archive_path, ok=False, message="Unsupported path type")

        if source.is_file():
            zip_single_file(source, archive_path)
        elif source.is_dir():
            zip_folder(source, archive_path, exclude_norms, include_norms)
        else:
            return BackupResult(source=source, archive=archive_path, ok=False, message="Unsupported path type")

        return BackupResult(source=source, archive=archive_path, ok=True)
    except Exception as exc:  # pragma: no cover - runtime safety path
        return BackupResult(source=source, archive=Path(), ok=False, message=str(exc))


def backup_one_worker(
    source_str: str,
    output_dir_str: str,
    stamp: str,
    exclude_norms: list[str],
    include_norms: list[str],
    dry_run: bool,
) -> BackupResult:
    return backup_one(
        Path(source_str),
        Path(output_dir_str),
        stamp,
        exclude_norms,
        include_norms,
        dry_run,
    )


def get_state_file(output_dir: Path) -> Path:
    return output_dir / STATE_FILE_NAME


def load_or_create_state(output_dir: Path) -> tuple[str, set[str]]:
    """Load resume state; create one if it does not exist."""
    state_file = get_state_file(output_dir)
    if state_file.exists():
        with state_file.open("r", encoding="utf-8") as f:
            data = json.load(f)
        stamp = data.get("stamp", datetime.now().strftime(TIME_FORMAT))
        done_items = set(data.get("done", []))
        return stamp, done_items

    stamp = datetime.now().strftime(TIME_FORMAT)
    save_state(output_dir, stamp, set())
    return stamp, set()


def save_state(output_dir: Path, stamp: str, done_items: set[str]) -> None:
    state_file = get_state_file(output_dir)
    data = {
        "stamp": stamp,
        "done": sorted(done_items),
        "updated_at": datetime.now().isoformat(timespec="seconds"),
    }
    with state_file.open("w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)


def clear_state(output_dir: Path) -> None:
    state_file = get_state_file(output_dir)
    if state_file.exists():
        state_file.unlink()


def render_progress(current: int, total: int, width: int = 28) -> str:
    if total <= 0:
        return "[----------------------------] 0/0 (0%)"
    ratio = current / total
    filled = int(ratio * width)
    bar = "#" * filled + "-" * (width - filled)
    percent = int(ratio * 100)
    return f"[{bar}] {current}/{total} ({percent}%)"


def main() -> int:
    start_time = datetime.now()

    ensure_output_dir(OUTPUT_DIR)
    backup_items, exclude_items, include_items = load_backup_config(CONFIG_PATH)
    exclude_paths = resolve_excludes(exclude_items, DEFAULT_EXCLUDE_BASE)
    include_paths = resolve_includes(include_items, DEFAULT_EXCLUDE_BASE)

    exclude_norms = [normalize_path(OUTPUT_DIR)]
    exclude_norms.extend(normalize_path(p) for p in exclude_paths)
    include_norms = [normalize_path(p) for p in include_paths]

    if DRY_RUN:
        stamp = datetime.now().strftime(TIME_FORMAT)
        done_items: set[str] = set()
    else:
        stamp, done_items = load_or_create_state(OUTPUT_DIR)

    results: list[BackupResult] = []
    total_items = len(backup_items)
    pending_sources: list[Path] = []

    print(f"Mode: {'DRY RUN' if DRY_RUN else 'REAL RUN'}", flush=True)
    print(f"Output dir: {OUTPUT_DIR}", flush=True)
    print(f"Config: {CONFIG_PATH}", flush=True)
    print(f"Exclude base: {DEFAULT_EXCLUDE_BASE}", flush=True)
    print(f"Start time: {start_time.strftime('%Y-%m-%d %H:%M:%S')}", flush=True)

    for index, item in enumerate(backup_items, start=1):
        source = resolve_item(item)

        if SHOW_PROGRESS:
            progress = render_progress(index - 1, total_items)
            sys.stdout.write(f"\r{progress} | Processing: {source}    ")
            sys.stdout.flush()

        if DRY_RUN:
            results.append(backup_one(source, OUTPUT_DIR, stamp, exclude_norms, include_norms, True))
            continue

        source_key = normalize_path(source)
        archive = OUTPUT_DIR / make_archive_name(source, stamp)

        if source_key in done_items:
            results.append(BackupResult(
                source=source,
                archive=archive,
                ok=True,
                message="Already done (resume)",
            ))
            continue

        if archive.exists():
            done_items.add(source_key)
            save_state(OUTPUT_DIR, stamp, done_items)
            results.append(BackupResult(
                source=source,
                archive=archive,
                ok=True,
                message="Archive exists, skipped",
            ))
            continue

        pending_sources.append(source)

    if not DRY_RUN and pending_sources:
        if MAX_WORKERS > 1:
            completed = 0
            with ProcessPoolExecutor(max_workers=MAX_WORKERS) as executor:
                future_map = {
                    executor.submit(
                        backup_one_worker,
                        str(src),
                        str(OUTPUT_DIR),
                        stamp,
                        exclude_norms,
                        include_norms,
                        False,
                    ): src
                    for src in pending_sources
                }
                for future in as_completed(future_map):
                    res = future.result()
                    results.append(res)
                    completed += 1
                    if SHOW_PROGRESS:
                        progress = render_progress(completed, len(pending_sources))
                        sys.stdout.write(f"\r{progress} | Completed workers    ")
                        sys.stdout.flush()
                    if res.ok:
                        source_key = normalize_path(res.source)
                        done_items.add(source_key)
                        save_state(OUTPUT_DIR, stamp, done_items)
        else:
            for source in pending_sources:
                res = backup_one(source, OUTPUT_DIR, stamp, exclude_norms, include_norms, False)
                if res.ok:
                    source_key = normalize_path(source)
                    done_items.add(source_key)
                    save_state(OUTPUT_DIR, stamp, done_items)
                results.append(res)

    if SHOW_PROGRESS:
        progress = render_progress(total_items, total_items)
        print(f"\r{progress} | Completed                                ")

    ok_count = sum(1 for r in results if r.ok)
    fail_count = len(results) - ok_count

    if not DRY_RUN and fail_count == 0:
        clear_state(OUTPUT_DIR)

    print(f"Backup finished. success={ok_count}, failed={fail_count}")
    for r in results:
        if r.ok:
            if r.message:
                print(f"[OK] {r.source} -> {r.archive} ({r.message})")
            else:
                print(f"[OK] {r.source} -> {r.archive}")
        else:
            print(f"[FAIL] {r.source} | reason: {r.message}")

    end_time = datetime.now()
    elapsed = (end_time - start_time).total_seconds()
    print(f"End time: {end_time.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"Elapsed seconds: {elapsed:.2f}")

    return 0 if fail_count == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
