use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

use nix::errno::Errno;

use crate::{Client, RequestId, StreamId};

/// One acknowledged terminal result. This request's destination access has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    request: RequestId,
    outcome: Result<Duration, i32>,
}

impl Completion {
    pub fn request(self) -> RequestId {
        self.request
    }

    /// Image-production time on CLOCK_MONOTONIC, or the negative completion errno.
    ///
    /// The timestamp is neither presentation nor dequeue time. Failed output
    /// may contain partial pixels; ended access alone does not establish validity.
    pub fn outcome(self) -> Result<Duration, i32> {
        self.outcome
    }
}

#[repr(C)]
#[derive(Default)]
struct ResultRecord {
    request: u64,
    completed_at_ns: i64,
    status: i32,
    reserved: u32,
}

#[repr(C)]
struct Dequeue {
    stream: u64,
    result: u64,
    reserved: u64,
}

nix::ioctl_write_ptr!(dequeue, b'd', 0x06, Dequeue);

impl Client {
    /// Acknowledge one result, or return None without waiting when the queue is empty.
    ///
    /// A successful syscall returns the request slot even when the frame failed.
    /// Only syscall EAGAIN means no result; terminal EAGAIN stays in the completion.
    /// Kernel copyout faults retain the record and slot, whereas malformed output
    /// detected after successful copyout has already been acknowledged and must
    /// be treated as a protocol failure, not retried as the same result.
    ///
    /// Poll readability is only an observation shared by all streams and duplicate
    /// descriptors. Drain each stream and handle revocation separately; POLLHUP
    /// does not imply there are no remaining terminal results.
    pub fn try_dequeue(&self, stream: StreamId) -> io::Result<Option<Completion>> {
        receive(|output| {
            let input = Dequeue {
                stream: stream.get(),
                result: (output as *mut ResultRecord) as u64,
                reserved: 0,
            };
            // SAFETY: The input points to the complete writable output, both live
            // through synchronous copyout. The ioctl wrapper does not retry EAGAIN.
            unsafe { dequeue(self.fd.as_raw_fd(), &input) }.map(|_| ())
        })
    }
}

fn receive(
    query: impl FnOnce(&mut ResultRecord) -> Result<(), Errno>,
) -> io::Result<Option<Completion>> {
    let mut output = ResultRecord::default();
    match query(&mut output) {
        Err(Errno::EAGAIN) => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(()) => (),
    }
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid capture completion");
    let request = RequestId::new(output.request).ok_or_else(invalid)?;
    if output.reserved != 0 || output.status > 0 || output.completed_at_ns < 0 {
        return Err(invalid());
    }
    let outcome = if output.status == 0 {
        Ok(Duration::from_nanos(output.completed_at_ns as u64))
    } else {
        if output.completed_at_ns != 0 {
            return Err(invalid());
        }
        Err(output.status)
    };
    Ok(Some(Completion { request, outcome }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn completion_layout_matches_the_kernel() {
        assert_eq!(size_of::<ResultRecord>(), 24);
        assert_eq!(offset_of!(ResultRecord, status), 16);
        assert_eq!(size_of::<Dequeue>(), 24);
        assert_eq!(offset_of!(Dequeue, result), 8);
        assert_eq!(nix::request_code_write!(b'd', 6, 24), 0x4018_6406);
    }

    #[test]
    fn empty_queue_does_not_retry_or_publish_partial_output() {
        let mut calls = 0;
        let result = receive(|output| {
            calls += 1;
            output.request = 99;
            Err(Errno::EAGAIN)
        })
        .unwrap();
        assert_eq!(result, None);
        assert_eq!(calls, 1);
    }

    #[test]
    fn terminal_error_is_a_result_not_an_empty_queue() {
        for status in [-nix::libc::EAGAIN, -nix::libc::ECANCELED, -nix::libc::EIO] {
            let result = receive(|output| {
                output.request = 17;
                output.status = status;
                Ok(())
            })
            .unwrap()
            .unwrap();
            assert_eq!(result.request().get(), 17);
            assert_eq!(result.outcome(), Err(status));
        }
    }

    #[test]
    fn capture_time_is_not_replaced_by_dequeue_time() {
        let result = receive(|output| {
            output.request = 1;
            output.completed_at_ns = 12345;
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(result.outcome(), Ok(Duration::from_nanos(12345)));
    }

    #[test]
    fn interrupted_or_faulted_calls_do_not_consume_partial_records() {
        for error in [Errno::EFAULT, Errno::EINTR, Errno::EKEYREVOKED] {
            assert_eq!(
                receive(|output| {
                    output.request = 1;
                    Err(error)
                })
                .unwrap_err()
                .raw_os_error(),
                Some(error as i32)
            );
        }
    }

    #[test]
    fn malformed_success_is_a_protocol_error() {
        for field in 0..5 {
            let result = receive(|output| {
                output.request = 1;
                match field {
                    0 => output.request = 0,
                    1 => output.reserved = 1,
                    2 => output.status = 1,
                    3 => output.completed_at_ns = -1,
                    _ => {
                        output.status = -5;
                        output.completed_at_ns = 1;
                    }
                }
                Ok(())
            });
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }
}
