use super::{
    memory::{split_protected, MemoryOp},
    LinuxTarget, PAGE_SIZE,
};
use nix::{sys::ptrace, unistd::Pid};
use std::{cmp, marker::PhantomData, mem};

/// Allows to read memory from different locations in debuggee's memory as a single operation.
pub struct ReadMemory<'a> {
    target: &'a LinuxTarget,
    read_ops: Vec<ReadOp>,
    /// This requires a mutable reference because we rewrite values of variables in `ReadOp`.
    _marker: PhantomData<&'a mut ()>,
}

impl<'a> ReadMemory<'a> {
    pub(in crate::target) fn new(target: &'a LinuxTarget) -> Self {
        ReadMemory {
            target,
            read_ops: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Reads a value of type `T` from debuggee's memory at location `remote_base`.
    /// This value will be written to the provided variable `val`.
    /// You should call `apply` in order to execute the memory read operation.
    /// The provided variable `val` can't be accessed until either `apply` is called or `self` is
    /// dropped.
    ///
    /// # Safety
    ///
    /// The type `T` must not have any invalid values.
    /// For example, `T` must not be a `bool`, as `transmute::<u8, bool>(2)` is not a valid value for a bool.
    /// In case of doubt, wrap the type in [`mem::MaybeUninit`].
    // todo: further document mem safety - e.g., what happens in the case of partial read
    pub unsafe fn read<T>(mut self, val: &'a mut T, remote_base: usize) -> Self {
        ReadOp::split_on_page_boundary(
            &ReadOp {
                remote_base,
                len: mem::size_of::<T>(),
                local_ptr: val as *mut T as *mut libc::c_void,
            },
            &mut self.read_ops,
        );
        self
    }

    /// Reads a value of type `*mut T` from debuggee's memory at location `remote_base`.
    /// This value will be written to the provided pointer `ptr`.
    /// You should call `apply` in order to execute the memory read operation.
    /// The provided pointer `ptr` can't be accessed until either `apply` is called or `self` is
    /// dropped.
    ///
    /// # Safety
    ///
    /// Memory location at `ptr` must be of valid size and must not be outlived by `ReadMem`.
    /// You need to ensure the lifetime guarantees, and generally you should prefer using `read<T>(&mut val)`.
    // todo: further document mem safety - e.g., what happens in the case of partial read
    pub unsafe fn read_ptr<T>(mut self, ptr: *mut T, remote_base: usize) -> Self {
        ReadOp::split_on_page_boundary(
            &ReadOp {
                remote_base,
                len: mem::size_of::<T>(),
                local_ptr: ptr as *mut _,
            },
            &mut self.read_ops,
        );
        self
    }

    /// Reads a slice of type `&mut [T]` from debuggee's memory at location `remote_base`.
    /// This value will be written to the provided slice `val`.
    /// You should call `apply` in order to execute the memory read operation.
    /// The provided value `val` can't be accessed until either `apply` is called or `self` is
    /// dropped.
    ///
    /// # Safety
    ///
    /// The type `T` must not have any invalid values.
    /// For example, `T` must not be a `bool`, as `transmute::<u8, bool>(2)` is not a valid value for a bool.
    /// In case of doubt, wrap the type in [`mem::MaybeUninit`].
    // todo: further document mem safety - e.g., what happens in the case of partial read
    pub unsafe fn read_slice<T>(mut self, val: &'a mut [T], remote_base: usize) -> Self {
        ReadOp::split_on_page_boundary(
            &ReadOp {
                remote_base,
                len: val.len() * mem::size_of::<T>(),
                local_ptr: val.as_mut_ptr() as *mut _,
            },
            &mut self.read_ops,
        );
        self
    }

    /// Reads a `u8` byte slice from debuggee's memory at location `remote_base`.
    /// This value will be written to the provided slice `val`.
    /// You should call `apply` in order to execute the memory read operation.
    pub fn read_byte_slice<T>(mut self, val: &'a mut [u8], remote_base: usize) -> Self {
        ReadOp::split_on_page_boundary(
            &ReadOp {
                remote_base,
                len: val.len(),
                local_ptr: val.as_mut_ptr() as *mut _,
            },
            &mut self.read_ops,
        );
        self
    }

    /// Executes the memory read operation.
    pub fn apply(self) -> Result<(), Box<dyn std::error::Error>> {
        let pid = self.target.pid;
        let read_len = self
            .read_ops
            .iter()
            .fold(0, |sum, read_op| sum + read_op.len);

        if read_len > isize::MAX as usize {
            panic!("Read size too big");
        };

        // FIXME: Probably a better way to do this
        let result = Self::read_process_vm(pid, &self.read_ops);

        if result.is_err() && result.unwrap_err() == nix::Error::Sys(nix::errno::Errno::EFAULT)
            || result.is_ok() && result.unwrap() != read_len as isize
        {
            let protected_maps = self
                .target
                .memory_maps()?
                .into_iter()
                .filter(|map| !map.is_readable)
                .collect::<Vec<_>>();

            let (protected, readable) =
                split_protected(&protected_maps, self.read_ops.into_iter())?;

            Self::read_process_vm(pid, &readable)?;
            Self::read_ptrace(pid, &protected)?;
        }
        Ok(())
    }

    /// Allows to read from several different locations with one system call.
    /// It will error on pages that are not readable. Returns number of bytes read at granularity of ReadOps.
    fn read_process_vm(pid: Pid, read_ops: &[ReadOp]) -> Result<isize, nix::Error> {
        let remote_iov = read_ops
            .iter()
            .map(|read_op| read_op.as_remote_iovec())
            .collect::<Vec<_>>();

        let local_iov = read_ops
            .iter()
            .map(|read_op| read_op.as_local_iovec())
            .collect::<Vec<_>>();

        let bytes_read = unsafe {
            // todo: document unsafety
            libc::process_vm_readv(
                pid.into(),
                local_iov.as_ptr(),
                local_iov.len() as libc::c_ulong,
                remote_iov.as_ptr(),
                remote_iov.len() as libc::c_ulong,
                0,
            )
        };

        if bytes_read == -1 {
            return Err(nix::Error::last());
        }

        Ok(bytes_read)
    }

    /// Allows to read from protected memory pages.
    /// This operation results in multiple system calls and is inefficient.
    fn read_ptrace(pid: Pid, read_ops: &[ReadOp]) -> Result<(), Box<dyn std::error::Error>> {
        let long_size = std::mem::size_of::<std::os::raw::c_long>();

        for read_op in read_ops {
            let mut offset: usize = 0;
            // Read until all of the data is read
            while offset < read_op.len {
                let data =
                    ptrace::read(pid, (read_op.remote_base + offset) as *mut std::ffi::c_void)?;

                // Read full word. No need to preserve other data
                if (read_op.len - offset) >= long_size {
                    // todo: document unsafety
                    unsafe {
                        *((read_op.local_ptr as usize + offset) as *mut i64) = data;
                    }

                // Read part smaller than word. Need to preserve other data
                } else {
                    // todo: document unsafety
                    unsafe {
                        let previous_bytes: &mut [u8] = std::slice::from_raw_parts_mut(
                            (read_op.local_ptr as usize + offset) as *mut u8,
                            read_op.len - offset,
                        );
                        let data_bytes = data.to_ne_bytes();

                        previous_bytes[0..(read_op.len - offset)]
                            .clone_from_slice(&data_bytes[0..(read_op.len - offset)]);
                    }
                }
                offset += long_size;
            }
        }
        Ok(())
    }
}

/// A single memory read operation.
#[derive(Debug, Clone, Copy)]
struct ReadOp {
    // Remote memory location.
    remote_base: usize,
    // Size of the `local_ptr` buffer.
    len: usize,
    // Pointer to a local destination buffer.
    local_ptr: *mut libc::c_void,
}

impl MemoryOp for ReadOp {
    fn remote_base(&self) -> usize {
        self.remote_base
    }
}

impl ReadOp {
    /// Converts the memory read operation into a remote IoVec.
    fn as_remote_iovec(&self) -> libc::iovec {
        libc::iovec {
            iov_base: self.remote_base as *const libc::c_void as *mut _,
            iov_len: self.len,
        }
    }

    /// Converts the memory read operation into a local IoVec.
    fn as_local_iovec(&self) -> libc::iovec {
        libc::iovec {
            iov_base: self.local_ptr,
            iov_len: self.len,
        }
    }

    /// Splits ReadOp so that each resulting ReadOp resides in only one memory page.
    fn split_on_page_boundary(&self, out: &mut Vec<ReadOp>) {
        // Number of bytes left to be read
        let mut left = self.len;

        let next_page_distance = *PAGE_SIZE - ((*PAGE_SIZE - 1) & self.remote_base);
        let to_next_read_op = cmp::min(left, next_page_distance);
        // Read from remote_base to the end or to the next page
        out.push(ReadOp {
            remote_base: self.remote_base,
            len: to_next_read_op,
            local_ptr: self.local_ptr,
        });
        left -= to_next_read_op;

        while left > 0 {
            if left < *PAGE_SIZE {
                // Read from beginning of the page to a part in the middle (last read)
                out.push(ReadOp {
                    remote_base: self.remote_base + (self.len - left),
                    len: left,
                    local_ptr: (self.local_ptr as usize + (self.len - left)) as *mut libc::c_void,
                });
                break;
            } else {
                // Whole page is being read
                out.push(ReadOp {
                    remote_base: self.remote_base + (self.len - left),
                    len: *PAGE_SIZE,
                    local_ptr: (self.local_ptr as usize + (self.len - left)) as *mut libc::c_void,
                });
                left -= *PAGE_SIZE;
            }
        }
    }
}
