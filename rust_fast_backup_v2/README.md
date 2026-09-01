# Rust Fast Backup v2

A Rust Windows ZIP backup tool with directory excludes, exception includes, Rayon-parallel top-level tasks, and resume after interruption.

Copy the config template from the project root:

```powershell
Copy-Item backup_items.example.yaml backup_items.yaml
```

Writes a real backup by default:

```powershell
Set-Location rust_fast_backup_v2
cargo run --release
```

Preview without creating ZIPs:

```powershell
$env:BACKUP_DRY_RUN="1"
cargo run --release
Remove-Item Env:BACKUP_DRY_RUN
```

Paths and config are set via `BACKUP_CONFIG`, `BACKUP_ROOT`, `BACKUP_EXCLUDE_BASE`, and `BACKUP_OUTPUT_DIR`. `target/` and generated executables do not enter the repository. Full documentation is in the root `README.md`.

The Rust default output directory is `C:\backup_rust_v2`. Override with `BACKUP_OUTPUT_DIR`.
