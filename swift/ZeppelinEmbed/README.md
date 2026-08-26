# ZeppelinEmbed for Swift

`ZeppelinStore` is the async Swift actor over the frozen Zeppelin Embed C ABI.
The package targets macOS 14 and iOS 17 or newer. Local source builds use the
system-library target; release consumers set `ZE_USE_XCFRAMEWORK=1` and use the
checksum-pinned XCFramework binary target in `Package.swift`.

Store databases in Application Support. `OpenOptions.excludeFromBackup`
defaults to `true`, which applies `NSURLIsExcludedFromBackupKey` to the store
directory. Do not put a store in iCloud Drive, Dropbox, or another synchronized
folder: the engine owns its rename, lock, and durability protocol.

The host owns maintenance scheduling. On iOS, submit a `BGProcessingTask`, open
the store, call `maintain(wallTimeNanoseconds:bytes:)` with explicit budgets,
then close before completing the task. The package does not register or schedule
background work.

For file protection, call `quiesce()` from
`protectedDataWillBecomeUnavailable`; call `resume()` after protected data is
available again. ABI v1 has no lightweight quiesce primitive, so these methods
intentionally close and reopen the same path, options, and epoch.
