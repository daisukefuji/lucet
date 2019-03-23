use crate::host;
use std::fs::File;
use std::os::unix::prelude::{FileTypeExt, FromRawFd, RawFd};

#[derive(Debug)]
pub struct FdEntry {
    pub fd_object: FdObject,
    pub rights_base: host::__wasi_rights_t,
    pub rights_inheriting: host::__wasi_rights_t,
}

impl FromRawFd for FdEntry {
    unsafe fn from_raw_fd(rawfd: RawFd) -> FdEntry {
        let (ty, mut rights_base, rights_inheriting) = {
            let file = File::from_raw_fd(rawfd);
            let ft = file.metadata().unwrap().file_type();
            // we just make a `File` here for convenience; we don't want it to close when it drops
            std::mem::forget(file);
            if ft.is_block_device() {
                (
                    host::__WASI_FILETYPE_BLOCK_DEVICE,
                    host::RIGHTS_BLOCK_DEVICE_BASE,
                    host::RIGHTS_BLOCK_DEVICE_INHERITING,
                )
            } else if ft.is_char_device() {
                if nix::unistd::isatty(rawfd).unwrap() {
                    (
                        host::__WASI_FILETYPE_CHARACTER_DEVICE,
                        host::RIGHTS_TTY_BASE,
                        host::RIGHTS_TTY_BASE,
                    )
                } else {
                    (
                        host::__WASI_FILETYPE_CHARACTER_DEVICE,
                        host::RIGHTS_CHARACTER_DEVICE_BASE,
                        host::RIGHTS_CHARACTER_DEVICE_INHERITING,
                    )
                }
            } else if ft.is_dir() {
                (
                    host::__WASI_FILETYPE_DIRECTORY,
                    host::RIGHTS_DIRECTORY_BASE,
                    host::RIGHTS_DIRECTORY_INHERITING,
                )
            } else if ft.is_file() {
                (
                    host::__WASI_FILETYPE_REGULAR_FILE,
                    host::RIGHTS_REGULAR_FILE_BASE,
                    host::RIGHTS_REGULAR_FILE_INHERITING,
                )
            } else if ft.is_socket() {
                use nix::sys::socket;
                match socket::getsockopt(rawfd, socket::sockopt::SockType).unwrap() {
                    socket::SockType::Datagram => (
                        host::__WASI_FILETYPE_SOCKET_DGRAM,
                        host::RIGHTS_SOCKET_BASE,
                        host::RIGHTS_SOCKET_INHERITING,
                    ),
                    socket::SockType::Stream => (
                        host::__WASI_FILETYPE_SOCKET_STREAM,
                        host::RIGHTS_SOCKET_BASE,
                        host::RIGHTS_SOCKET_INHERITING,
                    ),
                    s => panic!("unsupported socket type: {:?}", s),
                }
            } else if ft.is_fifo() {
                (
                    host::__WASI_FILETYPE_SOCKET_STREAM,
                    host::RIGHTS_SOCKET_BASE,
                    host::RIGHTS_SOCKET_INHERITING,
                )
            } else {
                panic!("unsupported file type: {:?}", ft);
            }
        };

        use nix::fcntl::{fcntl, OFlag, F_GETFL};
        let flags_bits = fcntl(rawfd, F_GETFL).expect("fcntl succeeds");
        let flags = OFlag::from_bits_truncate(flags_bits);
        let accmode = flags & OFlag::O_ACCMODE;
        if accmode == OFlag::O_RDONLY {
            rights_base &= !host::__WASI_RIGHT_FD_WRITE as host::__wasi_rights_t;
        } else if accmode == OFlag::O_WRONLY {
            rights_base &= !host::__WASI_RIGHT_FD_READ as host::__wasi_rights_t;
        }

        FdEntry {
            fd_object: FdObject {
                ty: ty as u8,
                rawfd,
            },
            rights_base,
            rights_inheriting,
        }
    }
}

#[derive(Debug)]
pub struct FdObject {
    pub ty: host::__wasi_filetype_t,
    pub rawfd: RawFd,
    // TODO: directories
}

impl Drop for FdObject {
    fn drop(&mut self) {
        nix::unistd::close(self.rawfd).unwrap_or_else(|e| eprintln!("FdObject::drop(): {}", e));
    }
}