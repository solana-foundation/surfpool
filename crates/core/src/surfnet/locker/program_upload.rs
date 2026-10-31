/// Allocated zeros are not received program bytes. The bitmap costs one bit per byte and
/// stays bounded by the same program-data limit as the upload itself.
#[derive(Clone, Default)]
pub(crate) struct ProgramUpload {
    bytes: Vec<u8>,
    received: Vec<u8>,
    filled: usize,
}

impl ProgramUpload {
    pub(super) fn stage(&mut self, offset: usize, end: usize, data: &[u8]) -> Option<Vec<u8>> {
        if !data.is_empty() {
            if self.bytes.len() < end {
                self.bytes.resize(end, 0);
                self.received.resize(end.div_ceil(8), 0);
            }
            self.bytes[offset..end].copy_from_slice(data);
            for index in offset..end {
                let mask = 1 << (index % 8);
                let byte = &mut self.received[index / 8];
                if *byte & mask == 0 {
                    self.filled += 1;
                    *byte |= mask;
                }
            }
        }
        (self.filled == self.bytes.len() && super::elf_is_complete(&self.bytes))
            .then(|| self.bytes.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::ProgramUpload;

    #[test]
    fn review_chunks_allow_reordering_and_overlap_without_counting_gaps() {
        let elf = crate::surfnet::noop_program::NOOP_PROGRAM_ELF;
        for reversed in [false, true] {
            let mut upload = ProgramUpload::default();
            let middle = elf.len() / 2;
            let chunks = [(0, 64), (middle, elf.len()), (32, middle + 1)];
            let order = if reversed { [1, 0, 2] } else { [0, 1, 2] };
            for (step, index) in order.into_iter().enumerate() {
                let (start, end) = chunks[index];
                let complete = upload.stage(start, end, &elf[start..end]);
                if step < 2 {
                    assert!(complete.is_none());
                } else {
                    assert_eq!(complete.as_deref(), Some(elf));
                }
            }
            assert_eq!(upload.filled, elf.len());
            assert_eq!(upload.stage(0, 64, &elf[..64]).as_deref(), Some(elf));
            assert_eq!(
                upload.filled,
                elf.len(),
                "overlap must not increment coverage"
            );
        }
    }
}
