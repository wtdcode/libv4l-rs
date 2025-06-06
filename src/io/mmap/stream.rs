use std::convert::TryInto;
use std::time::Duration;
use std::{io, mem, sync::Arc};

use crate::buffer::{Metadata, Type};
use crate::device::{Device, Handle};
use crate::io::mmap::arena::Arena;
use crate::io::traits::{CaptureStream, OutputStream, Stream as StreamTrait};
use crate::memory::Memory;
use crate::v4l2;
use crate::v4l_sys::*;

/// Stream of mapped buffers
///
/// An arena instance is used internally for buffer handling.
pub struct Stream<'a> {
    handle: Arc<Handle>,
    arena: Arena<'a>,
    arena_index: usize,
    buf_type: Type,
    buf_meta: Vec<Metadata>,
    timeout: Option<i32>,

    active: bool,
}

impl<'a> Stream<'a> {
    /// Returns a stream for frame capturing
    ///
    /// # Arguments
    ///
    /// * `dev` - Capture device ref to get its file descriptor
    /// * `buf_type` - Type of the buffers
    ///
    /// # Example
    ///
    /// ```
    /// use v4l::buffer::Type;
    /// use v4l::device::Device;
    /// use v4l::io::mmap::Stream;
    ///
    /// let dev = Device::new(0);
    /// if let Ok(dev) = dev {
    ///     let stream = Stream::new(&dev, Type::VideoCapture);
    /// }
    /// ```
    pub fn new(dev: &Device, buf_type: Type) -> io::Result<Self> {
        Stream::with_buffers(dev, buf_type, 4)
    }

    pub fn with_buffers(dev: &Device, buf_type: Type, buf_count: u32) -> io::Result<Self> {
        let mut arena = Arena::new(dev.handle(), buf_type);
        let count = arena.allocate(buf_count)?;
        let mut buf_meta = Vec::new();
        buf_meta.resize(count as usize, Metadata::default());

        Ok(Stream {
            handle: dev.handle(),
            arena,
            arena_index: 0,
            buf_type,
            buf_meta,
            active: false,
            timeout: None,
        })
    }

    /// Returns the raw device handle
    pub fn handle(&self) -> Arc<Handle> {
        self.handle.clone()
    }

    /// Sets a timeout of the v4l file handle.
    pub fn set_timeout(&mut self, duration: Duration) {
        self.timeout = Some(duration.as_millis().try_into().unwrap());
    }

    /// Clears the timeout of the v4l file handle.
    pub fn clear_timeout(&mut self) {
        self.timeout = None;
    }

    fn buffer_desc(&self) -> v4l2_buffer {
        let mut planes = v4l2_plane {
            .. unsafe { mem::zeroed() }
        };

        let mut desc = v4l2_buffer {
            type_: self.buf_type as u32,
            memory: Memory::Mmap as u32,
            ..unsafe { mem::zeroed() }
        };

        if self.buf_type as u32 == Type::VideoCaptureMplane as u32 {
            desc.length = 1;
            desc.m.planes = &mut planes;
        }

        return desc
    }
}

impl<'a> Drop for Stream<'a> {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            if let Some(code) = e.raw_os_error() {
                // ENODEV means the file descriptor wrapped in the handle became invalid, most
                // likely because the device was unplugged or the connection (USB, PCI, ..)
                // broke down. Handle this case gracefully by ignoring it.
                if code == 19 {
                    /* ignore */
                    return;
                }
            }

            panic!("{:?}", e)
        }
    }
}

impl<'a> StreamTrait for Stream<'a> {
    type Item = [u8];

    fn start(&mut self) -> io::Result<()> {
        unsafe {
            let mut typ = self.buf_type as u32;
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_STREAMON,
                &mut typ as *mut _ as *mut std::os::raw::c_void,
            )?;
        }

        self.active = true;
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        unsafe {
            let mut typ = self.buf_type as u32;
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_STREAMOFF,
                &mut typ as *mut _ as *mut std::os::raw::c_void,
            )?;
        }

        self.active = false;
        Ok(())
    }
}

impl<'a, 'b> CaptureStream<'b> for Stream<'a> {
    fn queue(&mut self, index: usize) -> io::Result<()> {
        let mut v4l2_buf = v4l2_buffer {
            index: index as u32,
            ..self.buffer_desc()
        };

        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_QBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )?;
        }

        Ok(())
    }

    fn dequeue(&mut self) -> io::Result<usize> {
        let mut v4l2_buf = self.buffer_desc();

        if self.handle.poll(libc::POLLIN, self.timeout.unwrap_or(-1))? == 0 {
            // This condition can only happen if there was a timeout.
            // A timeout is only possible if the `timeout` value is non-zero, meaning we should
            // propagate it to the caller.
            return Err(io::Error::new(io::ErrorKind::TimedOut, "VIDIOC_DQBUF"));
        }

        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_DQBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        self.arena_index = v4l2_buf.index as usize;

        self.buf_meta[self.arena_index] = Metadata {
            bytesused: v4l2_buf.bytesused,
            flags: v4l2_buf.flags.into(),
            field: v4l2_buf.field,
            timestamp: v4l2_buf.timestamp.into(),
            sequence: v4l2_buf.sequence,
        };

        Ok(self.arena_index)
    }

    fn next(&'b mut self) -> io::Result<(&Self::Item, &Metadata)> {
        if !self.active {
            // Enqueue all buffers once on stream start
            for index in 0..self.arena.bufs.len() {
                CaptureStream::queue(self, index)?;
            }

            self.start()?;
        } else {
            CaptureStream::queue(self, self.arena_index)?;
        }

        self.arena_index = CaptureStream::dequeue(self)?;

        // The index used to access the buffer elements is given to us by v4l2, so we assume it
        // will always be valid.
        let bytes = &self.arena.bufs[self.arena_index];
        let meta = &self.buf_meta[self.arena_index];
        Ok((bytes, meta))
    }
}

impl<'a, 'b> OutputStream<'b> for Stream<'a> {
    fn queue(&mut self, index: usize) -> io::Result<()> {
        let mut v4l2_buf = v4l2_buffer {
            index: index as u32,
            ..self.buffer_desc()
        };
        unsafe {
            // output settings
            //
            // MetaData.bytesused is initialized to 0. For an output device, when bytesused is
            // set to 0 v4l2 will set it to the size of the plane:
            // https://www.kernel.org/doc/html/v4.15/media/uapi/v4l/buffer.html#struct-v4l2-plane
            v4l2_buf.bytesused = self.buf_meta[index].bytesused;
            v4l2_buf.field = self.buf_meta[index].field;

            if self
                .handle
                .poll(libc::POLLOUT, self.timeout.unwrap_or(-1))?
                == 0
            {
                // This condition can only happen if there was a timeout.
                // A timeout is only possible if the `timeout` value is non-zero, meaning we should
                // propagate it to the caller.
                return Err(io::Error::new(io::ErrorKind::TimedOut, "VIDIOC_QBUF"));
            }

            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_QBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )
        }
    }

    fn dequeue(&mut self) -> io::Result<usize> {
        let mut v4l2_buf = self.buffer_desc();

        unsafe {
            v4l2::ioctl(
                self.handle.fd(),
                v4l2::vidioc::VIDIOC_DQBUF,
                &mut v4l2_buf as *mut _ as *mut std::os::raw::c_void,
            )?;
        }
        self.arena_index = v4l2_buf.index as usize;

        let bytesused = if self.buf_type as u32 == Type::VideoCaptureMplane as u32 {
            unsafe { (*v4l2_buf.m.planes).bytesused }
        } else {
            v4l2_buf.bytesused
        };

        self.buf_meta[self.arena_index] = Metadata {
            bytesused,
            flags: v4l2_buf.flags.into(),
            field: v4l2_buf.field,
            timestamp: v4l2_buf.timestamp.into(),
            sequence: v4l2_buf.sequence,
        };

        Ok(self.arena_index)
    }

    fn next(&'b mut self) -> io::Result<(&mut Self::Item, &mut Metadata)> {
        let init = !self.active;
        if !self.active {
            self.start()?;
        }

        // Only queue and dequeue once the buffer has been filled at the call site. The initial
        // call to this function from the call site will happen just after the buffers have been
        // allocated, meaning we need to return the empty buffer initially so it can be filled.
        if !init {
            OutputStream::queue(self, self.arena_index)?;
            self.arena_index = OutputStream::dequeue(self)?;
        }

        // The index used to access the buffer elements is given to us by v4l2, so we assume it
        // will always be valid.
        let bytes = &mut self.arena.bufs[self.arena_index];
        let meta = &mut self.buf_meta[self.arena_index];
        Ok((bytes, meta))
    }
}
