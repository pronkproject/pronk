use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use drm_capture::{Client, Description, Destination, OfferId, Plane, RequestId};
use pronk_dmabuf::{export_dependencies, Access};

use crate::names::Registration;
use crate::setup::{self, Setup};
use crate::worker::{Backend, Completed};
use crate::{invalid, Buffer, Config, Layout};

pub(crate) struct Native<F> {
    client: Client<F>,
    registration: Registration,
    stream_closed: bool,
    next_cleanup: usize,
}

impl<F: AsFd> Native<F> {
    pub(crate) fn open(
        client: Client<F>,
        buffers: &[Buffer],
        config: Config,
        registration: Registration,
        expected_offer: Option<OfferId>,
    ) -> io::Result<(Self, Layout)> {
        let offer = client.describe()?;
        if expected_offer.is_some_and(|expected| expected != offer.offer) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "capture offer changed while the generation was starting",
            ));
        }
        if offer.format != u32::from_le_bytes(*b"XR24") || offer.modifier != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "capture actor requires linear XRGB8888",
            ));
        }
        if config.capacity > offer.max_requests {
            return Err(invalid("capture capacity exceeds the current offer"));
        }
        setup::initialize(
            &StreamSetup {
                client: &client,
                registration: &registration,
                buffers,
                offer,
                capacity: config.capacity,
            },
            buffers.len(),
        )?;
        Ok((
            Self {
                client,
                registration,
                stream_closed: false,
                next_cleanup: 0,
            },
            Layout {
                width: offer.width,
                height: offer.height,
            },
        ))
    }
}

struct StreamSetup<'a, F> {
    client: &'a Client<F>,
    registration: &'a Registration,
    buffers: &'a [Buffer],
    offer: Description,
    capacity: std::num::NonZeroU32,
}

impl<F: AsFd> Setup for StreamSetup<'_, F> {
    fn open_stream(&self) -> io::Result<()> {
        self.client
            .open_stream(self.registration.stream, self.offer.offer, self.capacity)
    }

    fn register_destination(&self, slot: usize) -> io::Result<()> {
        let buffer = &self.buffers[slot];
        let planes = [Plane {
            buffer: buffer.as_fd(),
            stride: buffer.stride,
            offset: 0,
        }];
        self.client.register_destination(
            self.registration.destination(slot),
            &Destination {
                width: self.offer.width,
                height: self.offer.height,
                format: self.offer.format,
                modifier: self.offer.modifier,
                planes: &planes,
            },
        )
    }

    fn close_stream(&self) -> io::Result<()> {
        self.client.close_stream(self.registration.stream)
    }

    fn unregister_destination(&self, slot: usize) -> io::Result<()> {
        self.client
            .unregister_destination(self.registration.destination(slot))
    }
}

impl<F: AsFd + Send + 'static> Backend for Native<F> {
    type Owner = F;

    fn queue(&mut self, request: RequestId, slot: usize, buffer: &Buffer) -> io::Result<()> {
        let reuse = export_dependencies(buffer.as_fd(), Access::Write)?;
        self.client.queue_output(
            self.registration.stream,
            request,
            self.registration.destination(slot),
            Some(reuse.as_fd()),
        )
    }

    fn dequeue(&mut self) -> io::Result<Option<Completed>> {
        match self.client.try_dequeue(self.registration.stream)? {
            Some(result) => Ok(Some(Completed {
                request: result.request(),
                outcome: result.outcome(),
            })),
            None => {
                check_connection(self.client.as_fd())?;
                Ok(None)
            }
        }
    }

    fn close(&mut self) -> io::Result<()> {
        if !self.stream_closed {
            self.client.close_stream(self.registration.stream)?;
            self.stream_closed = true;
        }
        while self.next_cleanup < self.registration.destinations {
            self.client
                .unregister_destination(self.registration.destination(self.next_cleanup))?;
            self.next_cleanup += 1;
        }
        Ok(())
    }

    fn into_owner(self) -> F {
        self.client.into_owner()
    }
}

/// Observe terminal fd state after consuming any available stream result.
fn check_connection(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut descriptor = nix::libc::pollfd {
        fd: fd.as_raw_fd(),
        events: nix::libc::POLLIN,
        revents: 0,
    };
    // SAFETY: The borrowed descriptor stays live and poll receives one writable
    // entry. A zero timeout only samples terminal state; it never waits.
    let result = unsafe { nix::libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(error)
        };
    }
    if descriptor.revents & nix::libc::POLLNVAL != 0 {
        return Err(io::Error::from_raw_os_error(nix::libc::EBADF));
    }
    // A final result may arrive between dequeue and poll. Let the next pass
    // consume it before reporting the terminal authority state.
    if descriptor.revents & nix::libc::POLLIN != 0 {
        return Ok(());
    }
    if descriptor.revents & nix::libc::POLLHUP != 0 {
        return Err(io::Error::from_raw_os_error(nix::libc::EKEYREVOKED));
    }
    if descriptor.revents & nix::libc::POLLERR != 0 {
        return Err(io::Error::other("capture descriptor failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn terminal_observation_does_not_wait_for_data() {
        let (read, write) = nix::unistd::pipe().unwrap();
        let mut read = std::fs::File::from(read);
        let mut write = std::fs::File::from(write);
        check_connection(read.as_fd()).unwrap();
        write.write_all(b"result").unwrap();
        check_connection(read.as_fd()).unwrap();
        drop(write);
        check_connection(read.as_fd()).unwrap();
        let mut data = [0; 6];
        read.read_exact(&mut data).unwrap();
        assert_eq!(
            check_connection(read.as_fd()).unwrap_err().raw_os_error(),
            Some(nix::libc::EKEYREVOKED)
        );
    }
}
