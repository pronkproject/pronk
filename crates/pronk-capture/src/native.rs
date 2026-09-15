use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use drm_capture::{Client, Destination, DestinationId, Plane, RequestId, StreamId};
use pronk_dmabuf::{export_dependencies, Access};

use crate::worker::{Backend, Completed};
use crate::{invalid, Buffer, Config, Layout};

pub(crate) struct Native<F> {
    client: Client<F>,
    stream: StreamId,
    destinations: usize,
    stream_closed: bool,
    next_cleanup: usize,
}

impl<F: AsFd> Native<F> {
    pub(crate) fn open(
        client: Client<F>,
        buffers: &[Buffer],
        config: Config,
    ) -> io::Result<(Self, Layout)> {
        let offer = client.describe()?;
        if offer.format != u32::from_le_bytes(*b"XR24") || offer.modifier != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "capture actor requires linear XRGB8888",
            ));
        }
        if config.capacity > offer.max_requests {
            return Err(invalid("capture capacity exceeds the current offer"));
        }
        let stream = StreamId::new(1).unwrap();
        client.open_stream(stream, offer.offer, config.capacity)?;
        for (slot, buffer) in buffers.iter().enumerate() {
            let planes = [Plane {
                buffer: buffer.as_fd(),
                stride: buffer.stride,
                offset: 0,
            }];
            client.register_destination(
                destination(slot),
                &Destination {
                    width: offer.width,
                    height: offer.height,
                    format: offer.format,
                    modifier: offer.modifier,
                    planes: &planes,
                },
            )?;
        }
        Ok((
            Self {
                client,
                stream,
                destinations: buffers.len(),
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

fn destination(slot: usize) -> DestinationId {
    DestinationId::new(slot as u64 + 1).expect("bounded pool index")
}

impl<F: AsFd + Send + 'static> Backend for Native<F> {
    type Owner = F;

    fn queue(&mut self, request: RequestId, slot: usize, buffer: &Buffer) -> io::Result<()> {
        let reuse = export_dependencies(buffer.as_fd(), Access::Write)?;
        self.client
            .queue_output(self.stream, request, destination(slot), Some(reuse.as_fd()))
    }

    fn dequeue(&mut self) -> io::Result<Option<Completed>> {
        match self.client.try_dequeue(self.stream)? {
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
            self.client.close_stream(self.stream)?;
            self.stream_closed = true;
        }
        while self.next_cleanup < self.destinations {
            self.client
                .unregister_destination(destination(self.next_cleanup))?;
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
