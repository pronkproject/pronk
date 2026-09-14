use std::io;
use std::os::fd::AsFd;

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
        Ok(self
            .client
            .try_dequeue(self.stream)?
            .map(|result| Completed {
                request: result.request(),
                outcome: result.outcome(),
            }))
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
