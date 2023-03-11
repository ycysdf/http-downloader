use std::collections::Bound;
use std::num::NonZeroUsize;
use std::ops::RangeBounds;
use std::sync::Arc;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "async-graphql", derive(async_graphql::SimpleObject))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct RemainingChunks {
    pub chunk_size: usize,
    pub ranges: Vec<ChunkRange>,
}

#[allow(dead_code)]
enum ChunkHandle {
    Update {
        index: usize,
        range: ChunkRange,
    },
    Split {
        index: usize,
        left: ChunkRange,
        right: ChunkRange,
    },
    Remove {
        index: usize,
    },
    Error,
}

impl RemainingChunks {
    pub fn new(chunk_size: NonZeroUsize, total_len: u64) -> Self {
        Self {
            chunk_size: chunk_size.get(),
            ranges: vec![ChunkRange::from_len(0, total_len)],
        }
    }
    pub fn take_first(&mut self) -> Option<ChunkRange> {
        let chunk_size = self.chunk_size as u64;
        match self.ranges.first().map(|n| n.to_owned()) {
            None => None,
            Some(range) => {
                let len = match range.len() {
                    0 => {
                        self.handle(ChunkHandle::Remove { index: 0 });
                        return self.take_first();
                    }
                    len if len <= chunk_size => {
                        self.handle(ChunkHandle::Remove { index: 0 });
                        len
                    }
                    _ => {
                        self.handle(ChunkHandle::Update {
                            index: 0,
                            range: ChunkRange::new(range.start + chunk_size, range.end),
                        });
                        chunk_size
                    }
                };
                Some(ChunkRange::from_len(range.start, len))
            }
        }
    }
    /*    pub fn remove(&mut self, index: usize) -> bool {
            let r = 'r: {
                for (i, range) in self.0.iter().enumerate() {
                    if !range.contains(&index) {
                        continue;
                    }
                    let len = range.len();
                    if len == 1 {
                        break 'r ChunkHandle::Remove { index: i };
                    } else {
                        match index - range.start {
                            0 => {
                                break 'r ChunkHandle::Update {
                                    index: i,
                                    range: (index + 1)..range.end,
                                };
                            }
                            d if d == len => {
                                break 'r ChunkHandle::Update {
                                    index: i,
                                    range: range.start..(range.end - 1),
                                };
                            }
                            _ => {
                                break 'r ChunkHandle::Split {
                                    index,
                                    left: range.start..(index - 1),
                                    right: (index + 1)..range.end,
                                };
                            }
                        }
                    }
                }
                ChunkHandle::Error
            };
            self.handle(r);
            true
        }*/

    fn handle(&mut self, handle: ChunkHandle) -> bool {
        match handle {
            ChunkHandle::Update { index, range } => {
                self.ranges[index] = range;
            }
            ChunkHandle::Split { index, left, right } => {
                self.ranges[index] = right;
                self.ranges.insert(index, left);
            }
            ChunkHandle::Remove { index } => {
                self.ranges.remove(index);
            }
            ChunkHandle::Error => {
                return false;
            }
        }
        true
    }
}

#[cfg_attr(feature = "async-graphql", derive(async_graphql::SimpleObject))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct ChunkData {
    pub iter_count: usize,
    pub remaining: RemainingChunks,
    pub last_incomplete_chunks: Vec<ChunkInfo>,
}

impl ChunkData {
    pub fn remaining_len(&self) -> u64 {
        let mut len = 0;
        for x in &self.remaining.ranges {
            len += x.len();
        }
        for x in &self.last_incomplete_chunks {
            len += x.range.len();
        }
        len
    }

    pub fn no_chunk_remaining(&self) -> bool {
        self.remaining.ranges.is_empty()
    }

    pub fn next_chunk_range(&mut self) -> Option<ChunkInfo> {
        if let Some(chunk) = self.last_incomplete_chunks.pop() {
            return Some(chunk);
        }
        let range = self.remaining.take_first();
        if let Some(range) = range {
            self.iter_count += 1;
            let result = ChunkInfo {
                index: self.iter_count,
                range,
            };
            Some(result)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct ChunkIterator {
    pub content_length: u64,
    pub data: Arc<parking_lot::RwLock<ChunkData>>,
}

impl ChunkIterator {
    pub fn new(content_length: u64, data: ChunkData) -> Self {
        Self {
            content_length,
            data: Arc::new(parking_lot::RwLock::new(data)),
        }
    }


    pub fn next(&self) -> Option<ChunkInfo> {
        let mut data = self.data.write();
        data.next_chunk_range()
    }
}

#[cfg_attr(feature = "async-graphql", derive(async_graphql::SimpleObject))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub index: usize,
    pub range: ChunkRange,
}

// like RangeInclusive
#[cfg_attr(feature = "async-graphql", derive(async_graphql::SimpleObject), graphql(complex))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Copy, Clone)]
pub struct ChunkRange {
    pub start: u64,
    pub end: u64,
}

#[cfg(feature = "async-graphql")]
#[async_graphql::ComplexObject]
impl ChunkRange {
    #[graphql(name = "len")]
    async fn get_len(&self) -> u64 {
        self.len()
    }
}

impl ChunkRange {
    pub fn new(start: u64, end: u64) -> Self {
        debug_assert!(end >= start, "start: {},end:{}", start, end);
        Self {
            start,
            end,
        }
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        (self.end - self.start) + 1
    }

    pub fn to_range_header(&self) -> headers::Range {
        headers::Range::bytes(self).unwrap()
    }

    pub fn from_len(start: u64, len: u64) -> Self {
        Self {
            start,
            end: start + len - 1,
        }
    }
}

impl<'a> RangeBounds<u64> for &'a ChunkRange {
    fn start_bound(&self) -> Bound<&u64> {
        Bound::Included(&self.start)
    }

    fn end_bound(&self) -> Bound<&u64> {
        Bound::Included(&self.end)
    }
}
