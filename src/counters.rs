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
                        writeln!(out, "  {:<40} {}", $desc, self.$name).unwrap();
                    }
                )*
            }
        }
    };
}

counters! {
    dirs:                 "directories",
    files:                "regular files",
    symlinks:             "symlinks (not followed)",
    special:              "devices, fifos, sockets",
    hardlinks_skipped:    "hardlinks already counted",
    mappings_direct:      "direct extents",
    mappings_shared:      "shared (reflinked) extent slices",
    shared_single_ptr:    "shared slices seen with one pointer",
    single_ptr_resolved:  "  matched to their extent",
    single_ptr_unmatched: "  only ever seen with one pointer",
    bytes_single_ptr_unmatched: "    their bytes",
    single_ptr_ambiguous: "  overlapping several extents",
    bytes_single_ptr_ambiguous: "    their bytes (may be counted twice)",
    mappings_inline:      "inline data extents",
    bytes_inline:         "inline data bytes",
    mappings_delalloc:    "delayed allocation extents",
    bytes_delalloc:       "delayed allocation bytes",
    mappings_reserved:    "reservations (fallocate)",
    bytes_reserved:       "reserved bytes",
    mappings_unwritten:   "allocated, unwritten extents",
    mappings_encoded:     "encoded (compressed) extents",
    bytes_encoded:        "encoded bytes, uncompressed",
    mappings_unknown:     "extents of unknown location",
    mappings_unaligned:   "extents not on 512-byte sectors",
    fiemap_resumes:       "FIEMAP calls resumed after a full buffer",
    err_open:             "files that couldn't be opened",
    err_fiemap:           "files whose extents couldn't be read",
    err_readdir:          "directories that couldn't be read",
    err_stat:             "entries that couldn't be stat'ed",
}

impl Counters {
    pub fn errors(&self) -> u64 {
        self.err_open + self.err_fiemap + self.err_readdir + self.err_stat
    }
}
