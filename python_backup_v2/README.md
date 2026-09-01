# Python Backup v2

A stdlib Windows ZIP backup tool with directory excludes, exception includes, parallel top-level tasks, and resume after interruption.

Copy the config template from the project root:

```powershell
Copy-Item backup_items.example.yaml backup_items.yaml
```

Preview only by default:

```powershell
python python_backup_v2\migrate_backup_v2.py
```

After confirming sources, excludes, and output, run a real backup:

```powershell
$env:BACKUP_DRY_RUN="0"
python python_backup_v2\migrate_backup_v2.py
Remove-Item Env:BACKUP_DRY_RUN
```

Paths and config are set via `BACKUP_CONFIG`, `BACKUP_ROOT`, `BACKUP_EXCLUDE_BASE`, and `BACKUP_OUTPUT_DIR`. Full documentation is in the root `README.md`.
