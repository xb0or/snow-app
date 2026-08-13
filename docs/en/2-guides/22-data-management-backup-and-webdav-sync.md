# Data Management, Backup, and WebDAV Sync

The **Data management** entry at the top of Settings contains **Import & export**, **Backup & restore**, and **Cloud sync**. Database access, package files, and WebDAV requests stay in the main process or Rust layer; the renderer receives status without saved secrets.

## Import and export

1. Open **Data management → Import & export**.
2. Export the selected portable sections to a `.snow-config` file.
3. API keys, MCP environment variables and headers, proxy passwords, cookies, SSH credentials, and device paths are excluded by default.
4. If **Include sensitive configuration** is enabled, set an export password. The package uses Argon2id key derivation and AES-256-GCM encryption; the password cannot be recovered.
5. Import validates the format, paths, sizes, and SHA-256 hashes, creates a `pre-import` snapshot, and applies a merge or selected-section replacement in one SQLite transaction.

Workspace absolute paths, SSH paths, and system credentials in a package are not copied across devices. A wrong password or validation failure performs no database write; the safety snapshot created before import can be used for rollback.

## Database snapshots and automatic backups

**Backup & restore** uses SQLite Online Backup, so committed data in a live WAL is copied into a consistent database image. Each `.snowbackup` contains:

- `manifest.json` with the format, app version, schema, creation time, reason, scope, and file hashes;
- `database/snowapp.db`;
- `database/archive.db` by default.

Automatic backups support every 6 hours, every 12 hours, daily, or weekly schedules and retain 3–100 snapshots. The page also saves a custom directory, the `archive.db` choice, and whether to create safety snapshots before import and restore. This release does not place attachments, checkpoints, uploads, image-library files, or plugin runtime directories in the database snapshot.

Restore is staged and interruptible: the app validates the package, creates a `pre-restore` snapshot, writes staging data and `pending-restore.json`, then restarts. On the next startup, before database connections initialize, it rechecks staging, preserves the current databases, removes WAL/SHM sidecars, swaps the files, and runs the existing schema migration. Initialization failure restores the previous state from the rollback copy.

## WebDAV cloud sync

1. Enter an HTTPS WebDAV endpoint, remote root, username, and WebDAV password.
2. Set an independent sync-encryption password. It is never uploaded or returned to the renderer from preload.
3. Choose **Configuration sync** (recommended) or **Full database mirror**. Configuration sync is intended for routine multi-device use; mirror mode includes both databases and requires a restart after a pull.
4. Keep sync manual by default, or enable a 15-, 30-, or 60-minute interval.
5. Test the connection, then choose **Sync now**. When both devices changed, automatic sync pauses and offers keep local, use remote, or keep both.

The remote layout is `remoteRoot/snow-app/v1/`, with content-addressed objects named by SHA-256. Objects are encrypted before upload; `state.json` is updated with revision and ETag conditional writes. If the server has no ETag, the UI marks weak conflict protection and uses a revision second read to reduce silent overwrites. Authentication, offline, precondition, and quota failures preserve valid local data and appear as the latest error.

WebDAV rejects HTTP by default. Only the explicit **Allow insecure HTTP** option permits an unencrypted endpoint; production deployments should use HTTPS. A forgotten sync password cannot recover remote objects.

## Release migration note

This feature introduces a separate `manifest v1`, `.snow-config`, `.snowbackup`, and `userData/data-management/` storage domain. Upgrades do not automatically import legacy third-party configuration or upload an existing database; create a manual snapshot before first use. The current database schema is 31 and normal startup migrations remain in charge of database upgrades.

Browser passwords, cookies, SSH keys, system keys, plugin-private directories, and absolute workspace paths are never synchronized across devices. Attachment delta sync, remote object garbage collection, and row-level conversation merging are outside the first release.
