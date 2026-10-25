//! PipeWire explicit sync (`SPA_META_SyncTimeline`) for DMA-BUF screencasts.
//!
//! Each buffer gets one timeline syncobj, shared by the acquire and release data blocks (the
//! last two blocks of the buffer), like mutter does. Before queueing a buffer we move both points
//! forward: acquire = old release + 1, release = acquire + 1. We only queue buffers once
//! rendering has completed, so the acquire point is signaled right away from the CPU. The
//! consumer signals the release point when it's done; until then the buffer must not be reused.
//!
//! Some consumers (gnome-remote-desktop) require explicit sync whenever the render node supports
//! timeline syncobjs, so without this they cannot use our DMA-BUFs at all.
//!
//! See <https://docs.pipewire.org/page_dma_buf.html>.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;

use pipewire::spa::sys::{
    spa_buffer, spa_buffer_find_meta_data, spa_meta_sync_timeline, SPA_DATA_SyncObj,
    SPA_META_SyncTimeline, SPA_DATA_FLAG_READABLE,
};
use smithay::reexports::drm::control::{syncobj, Device as ControlDevice};
use smithay::reexports::drm::{Device, DriverCapability};

/// Number of extra data blocks a buffer has with explicit sync: acquire and release.
pub const SYNC_BLOCKS: usize = 2;

/// The render node, for syncobj ioctls.
#[derive(Debug)]
pub struct SyncDevice(OwnedFd);

impl AsFd for SyncDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for SyncDevice {}
impl ControlDevice for SyncDevice {}

/// A buffer's timeline syncobj.
#[derive(Debug)]
pub struct BufferTimeline {
    handle: syncobj::Handle,
    /// Exported syncobj fd, handed to the consumer in the acquire and release data blocks.
    fd: OwnedFd,
}

impl SyncDevice {
    /// Returns `None` if the device doesn't support timeline syncobjs.
    pub fn new(render_node: BorrowedFd<'_>) -> Option<Self> {
        let fd = match render_node.try_clone_to_owned() {
            Ok(fd) => fd,
            Err(err) => {
                warn!("error duplicating render node fd for explicit sync: {err:?}");
                return None;
            }
        };
        let device = Self(fd);

        let has_cap = |cap| {
            device
                .get_driver_capability(cap)
                .is_ok_and(|value| value != 0)
        };
        if has_cap(DriverCapability::SyncObj) && has_cap(DriverCapability::TimelineSyncObj) {
            Some(device)
        } else {
            debug!("render node doesn't support timeline syncobjs, no explicit sync");
            None
        }
    }

    pub fn create_timeline(&self) -> io::Result<BufferTimeline> {
        let handle = self.create_syncobj(false)?;
        match self.syncobj_to_fd(handle, false) {
            Ok(fd) => Ok(BufferTimeline { handle, fd }),
            Err(err) => {
                let _ = self.destroy_syncobj(handle);
                Err(err)
            }
        }
    }

    pub fn destroy_timeline(&self, timeline: BufferTimeline) {
        if let Err(err) = self.destroy_syncobj(timeline.handle) {
            warn!("error destroying syncobj: {err:?}");
        }
        // The fd closes on drop.
    }

    pub fn signal(&self, timeline: &BufferTimeline, point: u64) -> io::Result<()> {
        self.syncobj_timeline_signal(&[timeline.handle], &[point])
    }

    pub fn is_reached(&self, timeline: &BufferTimeline, point: u64) -> io::Result<bool> {
        let mut value = [0];
        self.syncobj_timeline_query(&[timeline.handle], &mut value, false)?;
        Ok(value[0] >= point)
    }
}

/// Returns the buffer's `SPA_META_SyncTimeline`, present when explicit sync was negotiated.
///
/// # Safety
///
/// `buffer` must point to a valid `spa_buffer`.
pub unsafe fn find_sync_timeline(
    buffer: *mut spa_buffer,
) -> Option<NonNull<spa_meta_sync_timeline>> {
    let size = size_of::<spa_meta_sync_timeline>();
    let p = spa_buffer_find_meta_data(buffer, SPA_META_SyncTimeline, size).cast();
    NonNull::new(p)
}

/// Fills the acquire and release data blocks (the last two) with the timeline syncobj.
///
/// # Safety
///
/// `buffer` must point to a valid `spa_buffer` with at least [`SYNC_BLOCKS`] data blocks.
pub unsafe fn fill_sync_blocks(buffer: *mut spa_buffer, timeline: &BufferTimeline) {
    use std::os::fd::AsRawFd as _;

    let n_datas = (*buffer).n_datas as usize;
    for i in n_datas - SYNC_BLOCKS..n_datas {
        let data = (*buffer).datas.add(i);
        (*data).type_ = SPA_DATA_SyncObj;
        (*data).flags = SPA_DATA_FLAG_READABLE;
        (*data).fd = timeline.fd.as_raw_fd() as i64;
        (*data).maxsize = 0;
    }
}
