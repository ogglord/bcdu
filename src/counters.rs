use std::fmt::Write;

macro_rules! counters {
    ($($name:ident: $desc:literal,)*) => {
        #[derive(Default, Debug, Clone, serde::Serialize)]
        pub struct Counters {
            $(pub $name: u64,)*
        }

        impl std::ops::AddAssign<&Counters> for Counters {
            fn add_assign(&mut self, o: &Counters) {
                $(self.$name += o.$name;)*
            }
        }

        impl Counters {
            pub fn to_text(&self, out: &mut String) {
                $(
                    if self.$name != 0 {
                        writeln!(out, "  {:<48} {}", $desc, self.$name).unwrap();
                    }
                )*
            }
        }
    };
}

counters! {
    dirs:                   "directories",
    files:                  "regular files",
    symlinks:               "symlinks (not followed)",
    special:                "devices, fifos, sockets",
    other_fs:               "directories on other filesystems (skipped)",
    hardlinks_skipped:      "hardlinks already counted",
    indirect_extents:       "indirect extents (reflink btree)",
    reflink_other:          "other reflink btree keys",
    extents_direct:         "direct extents",
    extents_reflinked:      "reflink pointers",
    refs_dangling:          "reflink pointers to missing extents",
    extents_other:          "other extents btree keys (whiteouts)",
    keys_unmapped_snapshot: "extent keys in snapshots of no walked subvolume",
    bytes_outside_direct:   "direct bytes of files outside the walk",
    bytes_inline:           "inline data bytes (in the btree)",
    bytes_reserved:         "reserved bytes (fallocate)",
    err_readdir:            "directories that couldn't be read",
    err_stat:               "entries that couldn't be stat'ed",
}

impl Counters {
    pub fn errors(&self) -> u64 {
        self.err_readdir + self.err_stat
    }
}
