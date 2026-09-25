use std::num::NonZeroU64;

pub(crate) trait GenerationOwned {
    fn generation(&self) -> NonZeroU64;
}

impl<T: GenerationOwned + ?Sized> GenerationOwned for Box<T> {
    fn generation(&self) -> NonZeroU64 {
        self.as_ref().generation()
    }
}

#[derive(Debug)]
pub(crate) enum GenerationSlot<T> {
    Empty { completed: Option<NonZeroU64> },
    Active(T),
}

impl<T: GenerationOwned> GenerationSlot<T> {
    pub(crate) fn empty() -> Self {
        Self::Empty { completed: None }
    }

    pub(crate) fn active(&self) -> Option<&T> {
        match self {
            Self::Active(active) => Some(active),
            Self::Empty { .. } => None,
        }
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Active(active) => Some(active),
            Self::Empty { .. } => None,
        }
    }

    pub(crate) fn completed(&self) -> Option<NonZeroU64> {
        match self {
            Self::Empty { completed } => *completed,
            Self::Active(_) => None,
        }
    }

    pub(crate) fn take_active(&mut self) -> Option<T> {
        let generation = self.active()?.generation();
        match std::mem::replace(
            self,
            Self::Empty {
                completed: Some(generation),
            },
        ) {
            Self::Active(active) => Some(active),
            Self::Empty { .. } => unreachable!("active slot replaced above"),
        }
    }
}
