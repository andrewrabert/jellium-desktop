//! SHM segment lifecycle.
//!
//! Wraps `memfd_create` + `mmap` plus the matching x11rb MIT-SHM
//! attach-fd/detach so a `ShmBuffer` ends up registered with the X server
//! and ready for `shm_put_image`.

use memmap2::{MmapMut, MmapOptions};
use nix::fcntl::{FcntlArg, SealFlag, fcntl};
use nix::sys::memfd::{MFdFlags, memfd_create};
use nix::unistd::ftruncate;
use x11rb::connection::Connection;
use x11rb::protocol::shm::{self, ConnectionExt as X11rbShmConnection};
use x11rb::rust_connection::RustConnection;

use crate::x11_state::ShmBuffer;

/// Allocate or reuse a SHM buffer at (w, h). Returns false on failure;
/// `buf` is left in its previous state when the reuse condition matched, or
/// in `empty()` state on failure.
pub fn shm_alloc(buf: &mut ShmBuffer, conn: &RustConnection, w: i32, h: i32) -> bool {
    let size: usize = (w as usize) * (h as usize) * 4;
    if buf.is_mapped() && buf.dims() == (w, h) {
        return true;
    }

    shm_free(buf, Some(conn));

    let Some((seg, map)) = attach_memfd(conn, size) else {
        return false;
    };
    buf.set(seg, map, w, h);
    true
}

/// Detaches the segment before unmapping, so the server never reads a
/// mapping this process has already dropped.
pub fn shm_free(buf: &mut ShmBuffer, conn: Option<&RustConnection>) {
    if !buf.is_mapped() {
        return;
    }
    if let Some(c) = conn
        && buf.seg() != 0
    {
        let _ = c.shm_detach(buf.seg());
    }
    buf.clear();
}

fn attach_memfd(conn: &RustConnection, size: usize) -> Option<(shm::Seg, MmapMut)> {
    let fd = memfd_create(
        c"jellium-shm",
        MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
    )
    .ok()?;
    ftruncate(&fd, size as i64).ok()?;
    // Sealed against grow/shrink so the server's mapping can never fault.
    fcntl(
        &fd,
        FcntlArg::F_ADD_SEALS(SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW),
    )
    .ok()?;
    // SAFETY: the memfd is private to this process until `shm_attach_fd`, and
    // sealing forbids any later resize, so the mapping stays valid.
    let map = unsafe { MmapOptions::new().len(size).map_mut(&fd) }.ok()?;
    let seg = conn.generate_id().ok()?;
    conn.shm_attach_fd(seg, fd, false).ok()?;
    Some((seg, map))
}
