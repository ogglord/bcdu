use std::ffi::c_void;
use std::os::fd::AsFd;

use rustix::io;
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode};

// An ioctl whose argument is a header followed by a flexible array: rustix's
// typed patterns size the value by its type, and these buffers are sized at
// runtime.
struct Raw<const OPCODE: Opcode>(*mut c_void);

unsafe impl<const OPCODE: Opcode> Ioctl for Raw<OPCODE> {
    type Output = ();
    const IS_MUTATING: bool = true;

    fn opcode(&self) -> Opcode {
        OPCODE
    }

    fn as_ptr(&mut self) -> *mut c_void {
        self.0
    }

    unsafe fn output_from_ptr(_: IoctlOutput, _: *mut c_void) -> io::Result<()> {
        Ok(())
    }
}

/// `arg` must point to a buffer laid out as the ioctl expects, at least as
/// large as the header says it is.
pub unsafe fn ioctl_buf<const OPCODE: Opcode, T>(fd: impl AsFd, arg: *mut T) -> io::Result<()> {
    unsafe { rustix::ioctl::ioctl(fd, Raw::<OPCODE>(arg.cast())) }
}
