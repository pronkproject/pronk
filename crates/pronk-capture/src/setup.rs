//! Roll back an incomplete stream before any capture request can be admitted.

use std::io;

pub(crate) trait Setup {
    fn open_stream(&self) -> io::Result<()>;
    fn register_destination(&self, slot: usize) -> io::Result<()>;
    fn close_stream(&self) -> io::Result<()>;
    fn unregister_destination(&self, slot: usize) -> io::Result<()>;
}

pub(crate) fn initialize(target: &impl Setup, destinations: usize) -> io::Result<()> {
    target.open_stream()?;
    let mut pending = Pending {
        target,
        registered: 0,
        armed: true,
    };
    for slot in 0..destinations {
        if let Err(cause) = target.register_destination(slot) {
            return Err(match pending.rollback() {
                None => cause,
                Some(cleanup) => io::Error::new(
                    cause.kind(),
                    format!("register capture destination: {cause}; cleanup failed: {cleanup}"),
                ),
            });
        }
        pending.registered += 1;
    }
    pending.armed = false;
    Ok(())
}

struct Pending<'a, T: Setup> {
    target: &'a T,
    registered: usize,
    armed: bool,
}

impl<T: Setup> Pending<'_, T> {
    fn rollback(&mut self) -> Option<io::Error> {
        if !self.armed {
            return None;
        }
        self.armed = false;
        let mut failure = self.target.close_stream().err();
        for slot in 0..self.registered {
            if let Err(error) = self.target.unregister_destination(slot) {
                failure.get_or_insert(error);
            }
        }
        failure
    }
}

impl<T: Setup> Drop for Pending<'_, T> {
    fn drop(&mut self) {
        let _ = self.rollback();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Open,
        Register(usize),
        Close,
        Unregister(usize),
    }

    #[derive(Default)]
    struct Target {
        calls: RefCell<Vec<Call>>,
        fail_open: bool,
        fail_register: Option<usize>,
        panic_register: Option<usize>,
        fail_cleanup: bool,
    }

    impl Setup for Target {
        fn open_stream(&self) -> io::Result<()> {
            self.calls.borrow_mut().push(Call::Open);
            if self.fail_open {
                Err(io::Error::from_raw_os_error(nix::libc::ESTALE))
            } else {
                Ok(())
            }
        }

        fn register_destination(&self, slot: usize) -> io::Result<()> {
            self.calls.borrow_mut().push(Call::Register(slot));
            assert_ne!(self.panic_register, Some(slot), "injected setup panic");
            if self.fail_register == Some(slot) {
                Err(io::Error::from_raw_os_error(nix::libc::ENOMEM))
            } else {
                Ok(())
            }
        }

        fn close_stream(&self) -> io::Result<()> {
            self.calls.borrow_mut().push(Call::Close);
            if self.fail_cleanup {
                Err(io::Error::other("close rejected"))
            } else {
                Ok(())
            }
        }

        fn unregister_destination(&self, slot: usize) -> io::Result<()> {
            self.calls.borrow_mut().push(Call::Unregister(slot));
            if self.fail_cleanup {
                Err(io::Error::other("removal rejected"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn rejected_stream_does_not_attempt_unknown_cleanup() {
        let target = Target {
            fail_open: true,
            ..Target::default()
        };
        assert_eq!(
            initialize(&target, 3).unwrap_err().raw_os_error(),
            Some(nix::libc::ESTALE)
        );
        assert_eq!(*target.calls.borrow(), [Call::Open]);
    }

    #[test]
    fn success_retains_the_stream_and_complete_pool() {
        let target = Target::default();
        initialize(&target, 2).unwrap();
        assert_eq!(
            *target.calls.borrow(),
            [Call::Open, Call::Register(0), Call::Register(1)]
        );
    }

    #[test]
    fn rejected_destination_removes_only_successfully_registered_slots() {
        let target = Target {
            fail_register: Some(2),
            ..Target::default()
        };
        assert_eq!(
            initialize(&target, 4).unwrap_err().raw_os_error(),
            Some(nix::libc::ENOMEM)
        );
        assert_eq!(
            *target.calls.borrow(),
            [
                Call::Open,
                Call::Register(0),
                Call::Register(1),
                Call::Register(2),
                Call::Close,
                Call::Unregister(0),
                Call::Unregister(1)
            ]
        );
    }

    #[test]
    fn rejecting_the_first_destination_still_closes_the_stream() {
        let target = Target {
            fail_register: Some(0),
            ..Target::default()
        };
        assert!(initialize(&target, 3).is_err());
        assert_eq!(
            *target.calls.borrow(),
            [Call::Open, Call::Register(0), Call::Close]
        );
    }

    #[test]
    fn cleanup_failure_does_not_skip_remaining_destinations() {
        let target = Target {
            fail_register: Some(2),
            fail_cleanup: true,
            ..Target::default()
        };
        let error = initialize(&target, 4).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::OutOfMemory);
        assert!(error.to_string().contains("cleanup failed: close rejected"));
        assert_eq!(
            &target.calls.borrow()[4..],
            [Call::Close, Call::Unregister(0), Call::Unregister(1)]
        );
    }

    #[test]
    fn unwinding_setup_also_retires_the_partial_registration() {
        let target = Target {
            panic_register: Some(1),
            ..Target::default()
        };
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| initialize(&target, 3)))
                .is_err()
        );
        assert_eq!(
            *target.calls.borrow(),
            [
                Call::Open,
                Call::Register(0),
                Call::Register(1),
                Call::Close,
                Call::Unregister(0)
            ]
        );
    }
}
