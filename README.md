# Windows Parallel ZIP Backup

A configurable ZIP backup tool for Windows, with Python and Rust v2 implementations. Both read the same backup plan. They can exclude large directories then re-include selected subdirectories, process top-level backup items in parallel, resume after interruption, and default to a safe preview (Python) or a real backup (Rust).

## Purpose

- Pack multiple files or directories into dated ZIPs, one archive per top-level source.
- Maintain backup sources, excludes, and exception includes in a text config.
- Shorten runtime on large trees by parallelizing top-level tasks.
- Skip already completed backup items after an interruption.
- Python defaults to dry-run; Rust defaults to writing real ZIPs.
- Provide a zero-third-party-dependency Python version and a high-performance Rust version.
- Do not store real user directories, personal file lists, or generated archives in the repository.

## Feature list

- `backup_items`: file and directory backup list.
- `exclude_items`: recursive exclude of directories or files.
- `include_items`: re-include selected subpaths under an excluded tree.
- Relative and absolute path input.
- `BACKUP_ROOT` as the relative source base.
- `BACKUP_EXCLUDE_BASE` as the relative exclude/include base.
- One dated ZIP per top-level source.
- Same-name sources are disambiguated with the parent directory.
- Empty directories are kept.
- Inaccessible files are logged as warnings and skipped.
- The output directory is auto-excluded so the tool does not back up its own archives.
- JSON resume-state file.
- Existing archives are treated as already complete.
- Progress, start time, end time, and elapsed time output.
- Python multiprocessing for top-level parallelism.
- Rust Rayon for top-level parallelism.
- Fast stored mode and large-buffer file copy.
- Python defaults to dry-run; Rust writes ZIPs by default and can preview via an environment variable.

## Tech stack and dependencies

### Python version

- Python 3.10+.
- Standard library only: `pathlib`, `zipfile`, `concurrent.futures`, `json`, and related modules.
- No `pip install` required.

### Rust version

- Rust stable, edition 2021.
- Cargo.
- `chrono`, `serde`, `serde_json`, `walkdir`, `zip`, `flate2`, `rayon`.
- A working MSVC or GNU Rust toolchain on Windows.

`Cargo.lock` is kept for rebuilds; `target/`, EXE, DLL, and debug symbols are excluded.

## Configuration

Copy the public template:

```powershell
Copy-Item backup_items.example.yaml backup_items.yaml
```

Real `backup_items.yaml` is excluded by `.gitignore` because it usually contains personal directory layout and private paths.

Structure:

```yaml
backup_items:
  - Documents

exclude_items:
  - AppData

include_items:
  - AppData\Roaming\ExampleApp\settings
```

Rules:

- Relative `backup_items` paths are based on `BACKUP_ROOT`.
- Relative `exclude_items` and `include_items` paths are based on `BACKUP_EXCLUDE_BASE`.
- Absolute paths are used as-is.
- An include can cross an excluded ancestor and restore only the chosen subtree.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `BACKUP_CONFIG` | local config at the project root | backup plan file |
| `BACKUP_ROOT` | system-drive root | relative backup-item base |
| `BACKUP_EXCLUDE_BASE` | current user home | relative exclude/include base |
| `BACKUP_OUTPUT_DIR` | Python: version-specific output under the user home; Rust: `C:\backup_rust_v2` | ZIP and resume-state location |
| `BACKUP_DRY_RUN` | Python default `1`; Rust default off | For Rust, `1` / `true` / `yes` previews only; for Python, `0` / `false` / `no` performs a real backup |

## Python usage

First preview:

```powershell
python python_backup_v2\migrate_backup_v2.py
```

Override default paths:

```powershell
$env:BACKUP_ROOT="SOURCE_ROOT"
$env:BACKUP_EXCLUDE_BASE="EXCLUDE_BASE"
$env:BACKUP_OUTPUT_DIR="OUTPUT_DIRECTORY"
python python_backup_v2\migrate_backup_v2.py
```

After confirming the preview, run a real backup:

```powershell
$env:BACKUP_DRY_RUN="0"
python python_backup_v2\migrate_backup_v2.py
Remove-Item Env:BACKUP_DRY_RUN
```

## Rust build and run

```powershell
Set-Location rust_fast_backup_v2
cargo build --release
cargo run --release
```

Rust writes real ZIPs by default. Preview without creating archives:

```powershell
$env:BACKUP_DRY_RUN="1"
cargo run --release
Remove-Item Env:BACKUP_DRY_RUN
```

If you start the program from another working directory, set `BACKUP_CONFIG` explicitly.

## Resume and state

A real backup creates `.backup_resume_state.json` in the output directory:

- Each completed top-level item records a normalized source path.
- The next run skips items already in the state.
- If the destination ZIP already exists, the item is also marked complete.
- After a full success the state file is deleted.
- Failed items remain in state so a later run can continue.

State files and ZIPs can expose file names and directory structure; do not commit them.

## Project file structure

```text
.
├── .gitignore
├── README.md
├── backup_items.example.yaml
├── python_backup_v2/
│   ├── README.md
│   └── migrate_backup_v2.py
└── rust_fast_backup_v2/
    ├── Cargo.toml
    ├── Cargo.lock
    ├── README.md
    └── src/
        └── main.rs
```

## Excluded local content

- Real backup plans and historical copies.
- Older Python/Rust implementations and migration scripts.
- Local Cargo mirror/proxy config and proxy install scripts.
- All Rust `target/` build caches and executables.
- Python virtualenvs and caches.
- Generated ZIP, 7z, RAR, tar, and similar archives.
- Output directories, resume state, logs, IDE config, and personal knowledge bases.

## Safety notes

- Before the first real Rust run, set `BACKUP_DRY_RUN=1` to preview sources, excludes, and output; the Python version is dry-run by default.
- Do not place the output directory inside a source tree and forget to exclude it; the program auto-excludes the current output directory but cannot recognize other historical output locations.
- Assess encryption and access control separately for private keys, credential stores, and browser profiles; ordinary ZIP is not encrypted.
- Write archives to a disk with enough space and controlled access, then verify they are readable.
