//! Monotonic stream and destination identities within one capture file.

use std::io;

use drm_capture::{DestinationId, StreamId};

use crate::invalid;

pub(crate) struct Names {
    next_stream: Option<u64>,
    next_destination: Option<u64>,
}

impl Default for Names {
    fn default() -> Self {
        Self {
            next_stream: Some(1),
            next_destination: Some(1),
        }
    }
}

/// Identities reserved for one actor, even if kernel setup is later rejected.
#[derive(Debug)]
pub(crate) struct Registration {
    pub stream: StreamId,
    first_destination: u64,
    pub destinations: usize,
}

impl Registration {
    pub fn destination(&self, slot: usize) -> DestinationId {
        assert!(
            slot < self.destinations,
            "slot belongs to the registered pool"
        );
        DestinationId::new(self.first_destination + slot as u64)
            .expect("the complete reserved range contains nonzero identities")
    }
}

impl Names {
    pub fn reserve(&mut self, destinations: usize) -> io::Result<Registration> {
        if !(1..=64).contains(&destinations) {
            return Err(invalid("invalid number of capture destinations"));
        }
        let exhausted = || io::Error::other("capture identities exhausted");
        let stream = self.next_stream.ok_or_else(exhausted)?;
        let first_destination = self.next_destination.ok_or_else(exhausted)?;
        let last_destination = first_destination
            .checked_add(destinations as u64 - 1)
            .ok_or_else(exhausted)?;
        self.next_stream = stream.checked_add(1);
        self.next_destination = last_destination.checked_add(1);
        Ok(Registration {
            stream: StreamId::new(stream).expect("stream identities start at one"),
            first_destination,
            destinations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_actors_use_distinct_names_within_one_file() {
        let mut names = Names::default();
        let first = names.reserve(4).unwrap();
        let replacement = names.reserve(3).unwrap();
        assert_eq!(first.stream.get(), 1);
        assert_eq!(replacement.stream.get(), 2);
        assert_eq!(first.destination(0).get(), 1);
        assert_eq!(first.destination(3).get(), 4);
        assert_eq!(replacement.destination(0).get(), 5);
        assert_eq!(replacement.destination(2).get(), 7);
    }

    #[test]
    fn abandoned_registration_does_not_make_names_reusable() {
        let mut names = Names::default();
        let _ = names.reserve(4).unwrap();
        let next = names.reserve(1).unwrap();
        assert_eq!(next.stream.get(), 2);
        assert_eq!(next.destination(0).get(), 5);
    }

    #[test]
    fn final_names_can_be_issued_once_without_wrapping() {
        let mut names = Names {
            next_stream: Some(u64::MAX),
            next_destination: Some(u64::MAX - 2),
        };
        let last = names.reserve(3).unwrap();
        assert_eq!(last.stream.get(), u64::MAX);
        assert_eq!(last.destination(2).get(), u64::MAX);
        assert!(names.reserve(1).is_err());
    }

    #[test]
    fn an_unrepresentable_pool_does_not_partially_advance_names() {
        let mut names = Names {
            next_stream: Some(7),
            next_destination: Some(u64::MAX),
        };
        assert!(names.reserve(2).is_err());
        let last = names.reserve(1).unwrap();
        assert_eq!(last.stream.get(), 7);
        assert_eq!(last.destination(0).get(), u64::MAX);
        assert!(names.reserve(1).is_err());
    }
}
