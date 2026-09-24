//! Immediate liveness monitoring for a classified PipeWire connection.
//!
//! PipeWire callbacks are normally sufficient to report a remote disconnect,
//! but a daemon restart can leave a client loop blocked before it observes the
//! socket hangup. This monitor waits directly on a duplicated connection FD
//! and forwards a disconnect notification into the owning PipeWire loop.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread::{self, JoinHandle};

pub(crate) struct RemoteDisconnectMonitor {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl RemoteDisconnectMonitor {
    pub(crate) fn spawn(
        remote: OwnedFd,
        notify: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        // SAFETY: `eventfd` creates a new close-on-exec, nonblocking Linux FD.
        // Its ownership moves immediately into `OwnedFd` after checking the
        // result for the documented negative-error convention.
        let stop_raw =
            unsafe { nix::libc::eventfd(0, nix::libc::EFD_CLOEXEC | nix::libc::EFD_NONBLOCK) };
        if stop_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `stop_raw` is a newly created, uniquely owned eventfd.
        let stop = unsafe { OwnedFd::from_raw_fd(stop_raw) };
        let stop_for_thread = stop.try_clone()?;
        let thread = thread::Builder::new()
            .name("pronk-pipewire-remote-monitor".to_string())
            .spawn(move || wait_for_disconnect(remote, stop_for_thread, notify))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    fn stop(&mut self) {
        let value = 1_u64;
        // SAFETY: `stop` is a valid eventfd, and `value` remains valid for the
        // complete fixed-size write. The descriptor is nonblocking, so teardown
        // never waits for the monitor thread to receive the wakeup.
        let _ = unsafe {
            nix::libc::write(
                self.stop.as_raw_fd(),
                (&value as *const u64).cast(),
                std::mem::size_of_val(&value),
            )
        };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RemoteDisconnectMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_for_disconnect(remote: OwnedFd, stop: OwnedFd, notify: impl FnOnce()) {
    let mut descriptors = [
        nix::libc::pollfd {
            fd: remote.as_raw_fd(),
            events: 0,
            revents: 0,
        },
        nix::libc::pollfd {
            fd: stop.as_raw_fd(),
            events: nix::libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `descriptors` is a valid contiguous array of `pollfd`
        // records retained for this call. The timeout is infinite, and the
        // paired eventfd gives teardown an immediate wakeup path.
        let result = unsafe {
            nix::libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as nix::libc::nfds_t,
                -1,
            )
        };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if descriptors[1].revents != 0 {
            return;
        }
        let disconnected =
            nix::libc::POLLERR | nix::libc::POLLHUP | nix::libc::POLLNVAL | nix::libc::POLLRDHUP;
        if descriptors[0].revents & disconnected != 0 {
            notify();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn peer_close_notifies_the_owner() {
        let (local, peer) = UnixStream::pair().unwrap();
        let (notified, receiver) = mpsc::channel();
        let _monitor = RemoteDisconnectMonitor::spawn(
            // SAFETY: `into_raw_fd` transfers the uniquely owned socket FD.
            unsafe { OwnedFd::from_raw_fd(local.into_raw_fd()) },
            move || notified.send(()).unwrap(),
        )
        .unwrap();
        drop(peer);
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn teardown_wakes_the_monitor_without_a_disconnect_notification() {
        let (local, _peer) = UnixStream::pair().unwrap();
        let (notified, receiver) = mpsc::channel();
        let monitor = RemoteDisconnectMonitor::spawn(
            // SAFETY: `into_raw_fd` transfers the uniquely owned socket FD.
            unsafe { OwnedFd::from_raw_fd(local.into_raw_fd()) },
            move || notified.send(()).unwrap(),
        )
        .unwrap();
        drop(monitor);
        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
    }
}
