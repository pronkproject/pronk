//! Several accepted source reads represented by one native completion.

use std::io;

use pronk_dmabuf::SyncFile;
use pronk_gpu::vulkan::{PendingPrivateRead, PrivateImage};

/// Accepted native reads that belong to one renderer source claim.
///
/// Each pending read retains its imported source and private destination until
/// native completion. The borrowed completion represents every submitted read;
/// it can be transferred to the source provider before pixel waits begin.
#[must_use = "transfer the completion before waiting for private pixels"]
pub struct SubmittedReads {
    reads: Vec<PendingPrivateRead>,
    merged: Option<SyncFile>,
}

impl SubmittedReads {
    /// Collect accepted reads and prepare one aggregate completion record.
    ///
    /// An empty collection is not a submitted source claim. Failure retains all
    /// pending owners because dropping them may need to retire native work.
    pub fn new(reads: Vec<PendingPrivateRead>) -> Result<Self, ReadCollectionError> {
        if reads.is_empty() {
            return Err(ReadCollectionError {
                reads,
                error: invalid("a submitted source claim must contain a read"),
            });
        }
        let mut completions = reads.iter().filter_map(PendingPrivateRead::completion);
        let first = completions.next();
        let second = completions.next();
        let merged = match (first, second) {
            (Some(first), Some(second)) => {
                let mut merged = match first.merge(second) {
                    Ok(merged) => merged,
                    Err(error) => return Err(ReadCollectionError { reads, error }),
                };
                for completion in completions {
                    merged = match merged.merge(completion) {
                        Ok(merged) => merged,
                        Err(error) => return Err(ReadCollectionError { reads, error }),
                    };
                }
                Some(merged)
            }
            _ => None,
        };
        Ok(Self { reads, merged })
    }

    /// Completion for every submitted read, if native work remains represented.
    ///
    /// `None` means every submission returned its already-completed sentinel.
    /// With one pending record, the original sync file is borrowed directly.
    pub fn completion(&self) -> Option<&SyncFile> {
        self.merged
            .as_ref()
            .or_else(|| self.reads.iter().find_map(PendingPrivateRead::completion))
    }

    pub fn len(&self) -> usize {
        self.reads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }

    /// Wait for valid pixels from every read in submission order.
    ///
    /// A completion error invalidates the complete collection. Remaining native
    /// owners are retired by their drop path; no partial image list is returned.
    pub fn wait(self) -> io::Result<Vec<PrivateImage>> {
        self.reads
            .into_iter()
            .map(PendingPrivateRead::wait)
            .collect()
    }
}

/// Failure to represent accepted reads with one native completion.
pub struct ReadCollectionError {
    reads: Vec<PendingPrivateRead>,
    error: io::Error,
}

impl std::fmt::Debug for ReadCollectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadCollectionError")
            .field("read_count", &self.reads.len())
            .field("error", &self.error)
            .finish()
    }
}

impl std::fmt::Display for ReadCollectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "collect native source reads: {}", self.error)
    }
}

impl std::error::Error for ReadCollectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl ReadCollectionError {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parts(self) -> (Vec<PendingPrivateRead>, io::Error) {
        (self.reads, self.error)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use pronk_dmabuf::Completion;
    use pronk_gpu::vulkan::Device;

    use super::*;

    #[test]
    fn empty_collection_is_not_submitted_work() {
        let error = SubmittedReads::new(Vec::new()).err().unwrap();
        assert_eq!(error.error().kind(), io::ErrorKind::InvalidInput);
        assert!(error.into_parts().0.is_empty());
    }

    #[test]
    #[ignore = "requires explicit Vulkan GPU and modifier selection"]
    fn several_native_reads_have_one_successful_completion() {
        let node = std::env::var_os("PRONK_GPU_RENDER_NODE").expect("select render node");
        let modifier = std::env::var("PRONK_GPU_MODIFIER").expect("select hex modifier");
        let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).unwrap();
        let producer = Device::open(&node).unwrap();
        let worker = Device::open(node).unwrap();
        let size = NonZeroU32::new(64).unwrap();
        let mut originals = Vec::new();
        let mut reads = Vec::new();
        for color in [[17, 85, 204], [231, 57, 19], [30, 90, 180]] {
            let (image, completion) = producer
                .allocate(size, size, modifier)
                .unwrap()
                .clear_waited(color)
                .unwrap();
            // SAFETY: Exact compatible allocator metadata and a completed
            // foreign release. The original remains unchanged through reading.
            let source = unsafe {
                worker.import_source(image.export().unwrap(), image.layout(), completion)
            }
            .unwrap();
            reads.push(
                source
                    .submit_private_copy(worker.allocate_private(size, size).unwrap())
                    .unwrap(),
            );
            originals.push(image);
        }
        let submitted = match SubmittedReads::new(reads) {
            Ok(submitted) => submitted,
            Err(_) => panic!("native read completions did not merge"),
        };
        assert_eq!(submitted.len(), 3);
        assert!(!submitted.is_empty());
        let completion = submitted
            .completion()
            .expect("native reads returned no completion record");
        assert!(matches!(
            completion.completion().unwrap(),
            None | Some(Completion::Success)
        ));
        assert_eq!(submitted.wait().unwrap().len(), 3);
        for image in originals {
            image.clear_waited([255; 3]).unwrap();
        }
    }
}
