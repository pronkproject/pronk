use std::num::NonZeroU32;

use crate::{AudioNodeIdentity, VideoNodeIdentity};

pub(crate) trait RegisteredNode {
    fn object_id(&self) -> NonZeroU32;
}

impl RegisteredNode for VideoNodeIdentity {
    fn object_id(&self) -> NonZeroU32 {
        self.object_id
    }
}

impl RegisteredNode for AudioNodeIdentity {
    fn object_id(&self) -> NonZeroU32 {
        self.object_id
    }
}

/// Registry and stream callbacks can arrive in either order. A matched node
/// is the only state that may complete source startup.
#[derive(Debug)]
pub(crate) enum NodeRegistration<I> {
    AwaitingBoth,
    AwaitingRegistry(NonZeroU32),
    AwaitingStream(I),
    Matched(I),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NodeIdMismatch {
    pub(crate) registry: NonZeroU32,
    pub(crate) stream: NonZeroU32,
}

impl<I: Clone + RegisteredNode> NodeRegistration<I> {
    pub(crate) fn observe_registry(&mut self, identity: I) -> Result<Option<I>, NodeIdMismatch> {
        let stream_id = match self {
            Self::AwaitingRegistry(id) => Some(*id),
            Self::Matched(current) => Some(current.object_id()),
            Self::AwaitingBoth | Self::AwaitingStream(_) => None,
        };
        if let Some(stream) = stream_id {
            if identity.object_id() != stream {
                return Err(NodeIdMismatch {
                    registry: identity.object_id(),
                    stream,
                });
            }
            *self = Self::Matched(identity.clone());
            return Ok(Some(identity));
        }
        *self = Self::AwaitingStream(identity);
        Ok(None)
    }

    pub(crate) fn observe_stream(
        &mut self,
        stream: NonZeroU32,
    ) -> Result<Option<I>, NodeIdMismatch> {
        if let Some(identity) = self.identity().cloned() {
            if identity.object_id() != stream {
                return Err(NodeIdMismatch {
                    registry: identity.object_id(),
                    stream,
                });
            }
            *self = Self::Matched(identity.clone());
            return Ok(Some(identity));
        }
        *self = Self::AwaitingRegistry(stream);
        Ok(None)
    }

    pub(crate) fn identity(&self) -> Option<&I> {
        match self {
            Self::AwaitingStream(identity) | Self::Matched(identity) => Some(identity),
            Self::AwaitingBoth | Self::AwaitingRegistry(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Identity(NonZeroU32);

    impl RegisteredNode for Identity {
        fn object_id(&self) -> NonZeroU32 {
            self.0
        }
    }

    fn id(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    #[test]
    fn startup_accepts_registry_and_stream_in_either_order() {
        let mut registry_first = NodeRegistration::AwaitingBoth;
        assert_eq!(registry_first.observe_registry(Identity(id(7))), Ok(None));
        assert_eq!(registry_first.identity(), Some(&Identity(id(7))));
        assert_eq!(
            registry_first.observe_stream(id(7)),
            Ok(Some(Identity(id(7))))
        );

        let mut stream_first = NodeRegistration::AwaitingBoth;
        assert_eq!(stream_first.observe_stream(id(8)), Ok(None));
        assert_eq!(stream_first.identity(), None);
        assert_eq!(
            stream_first.observe_registry(Identity(id(8))),
            Ok(Some(Identity(id(8))))
        );
    }

    #[test]
    fn mismatch_does_not_replace_the_registered_identity() {
        let mut registration = NodeRegistration::AwaitingBoth;
        registration.observe_registry(Identity(id(7))).unwrap();
        assert_eq!(
            registration.observe_stream(id(8)),
            Err(NodeIdMismatch {
                registry: id(7),
                stream: id(8)
            })
        );
        assert_eq!(registration.identity(), Some(&Identity(id(7))));
    }
}
