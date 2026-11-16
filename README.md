# bcdu

What each folder really occupies on bcachefs.

`du` adds up file sizes. On bcachefs, files often share their data: `cp
--reflink` (the default for `cp` since coreutils 9), deduplicating
importers like Sonarr and Radarr, and hardlinks. `du` counts shared data
again for every file, so a media folder holding a download and its imported
copy can look twice its real size, and the folder sizes add up to more than
the filesystem holds.

bcdu reads bcachefs's extents and reflink btrees instead and counts every
extent once. For each folder it reports:

| column    | meaning |
|-----------|---------|
| total     | what the folder's files occupy, shared data counted once |
| exclusive | what only the folder's files hold: what deleting it would free |
| shared    | total − exclusive: data also held by files elsewhere |
| apparent  | the file sizes, as `du -b` reports them |

```
$ sudo bcdu -d 1 /pool/tmp/t
    total  exclusive     shared   apparent  path
370147328  370147328          0  905969664  /pool/tmp/t  [subvolume 4]
369098752   67108864  301989888  369098752    two
303038464    1048576  301989888  570425344    one

bcachefs user data, one copy:            1590863286272
  in files under this path:              370147328
  only in files outside it:              1590493138944
```

Here `one` and `two` share a 256 MiB file through reflinks and a 32 MiB file
through a hardlink: deleting `one` would free only its 1 MiB of its own.

The summary accounts for all of bcachefs's user data. Run from the mount
point, "in files under this path" is the whole of it, to the byte.

## Usage

```
bcdu [OPTIONS] [PATH]

  -d, --depth <N>        folder levels to show below PATH [default: 2]
  -h, --human-readable   human-readable units
      --json             output JSON
      --check            compare each subvolume's never-reflinked extents
                         with bcachefs's own accounting of them
  -j, --threads <N>      threads [default: one per CPU]
  -v, --verbose          print counters and timings
```

On a 1.6 TB pool with 420,000 files and 7 million reflinked extents, a full
run takes about 3 seconds.

## Requirements

- bcachefs 1.39 or newer, for `BCH_IOCTL_QUERY_BTREE_KEYS`
- Linux 6.10 or newer, for the subvolume id in `statx`
- root: the btrees are read through ioctls that need it
- a little endian machine

## Install

```
nix run github:ogglord/bcdu -- -h /mnt
```

or `cargo install --path .` from a checkout.

## How it works

See [doc/how-it-works.md](doc/how-it-works.md).

## Licence

MIT OR Apache-2.0
