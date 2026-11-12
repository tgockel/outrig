# A failed nft apply reports a table that was never made as left behind

Found in review of #912; present before it.

When nft rejects the apply transaction in the right namespace, it creates no table. The undo armed
before the apply deletes by name (`delete_nft_table`), so it fails with ENOENT. The attach then
reports `NetworkAttachNotUndone` even when the resolver was restored and no redirect remains.

The name-based undo should count a table that is confirmed missing as cleaned up, and still
report every other failure. Test an apply failure followed by a missing-table delete.
