# How bcdu works

## Where a file's data is

bcachefs keeps a file's data in the extents btree, as keys of the file's
inode and snapshot. A key is one of:

- an **extent**: pointers to the data on disk, owned by this file alone
- a **reflink pointer**: an index range into the reflink btree, whose
  **indirect extents** hold data several files can share

The first time a file's data is reflinked, by `cp --reflink` for example,
bcachefs moves its extents into the reflink btree and leaves reflink
pointers behind. Each indirect extent is refcounted as a whole: it stays
allocated, all of it, while any reflink pointer reaches into any part of it.

## What bcdu does

1. Walks the directory tree, with `statx` on every file for its inode
   number, size and subvolume. Symlinks aren't followed, and other
   filesystems mounted below are skipped.
2. Reads the reflink btree: every indirect extent.
3. Reads the extents btree, split between threads by inode number. An
   extent adds to its file's own bytes; a reflink pointer records which
   indirect extents the file holds, including the pads bcachefs keeps on
   a pointer when an indirect extent is split.
4. Charges each indirect extent to the folders of the files holding it.

Both btrees are read through `BCH_IOCTL_QUERY_BTREE_KEYS` on the mounted
filesystem. Nothing is read from the devices directly.

## How shared data is charged

For data held by several files, each folder that contains one of them is
charged it in **total**, once however many of its files hold it. Only a
folder that contains *all* of them is charged it in **exclusive**. So for
any folder, exclusive is what deleting it would free, and total is what it
needs on its own.

A hardlinked file is one inode with names in several folders: its data is
charged the same way, shared between them.

Data also held by a file outside the walked path is in total but never in
exclusive: deleting the folder wouldn't free it.

The figures are for one copy of the data, as `du` reports. With
`data_replicas=2` the disks hold twice as much.

## Checking it

The summary splits bcachefs's own count of user data (from
`BCH_IOCTL_QUERY_ACCOUNTING`) into what the walk found, what only files
outside the path hold, and indirect extents nothing references. Whatever
doesn't fit is shown as unexplained.

`--check` compares each subvolume's never-reflinked extents with what
bcachefs accounts to the subvolume's snapshot. bcachefs's per-subvolume
counter leaves out reflinked data entirely, which is why it can't answer
the question on its own.

## Limits

- **Compression**: sizes are uncompressed. On a compressed filesystem the
  summary shows the difference as unexplained.
- **Snapshots**: only the subvolumes the walk passes through are mapped to
  their snapshots. Extents in other snapshots, including the ones a
  snapshot shares with its subvolume, count as outside the walk.
- **Inline data** lives in the btree, not in user data, and isn't counted.
