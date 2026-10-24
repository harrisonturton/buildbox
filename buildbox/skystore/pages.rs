//! Large pages that stored bytes are packed into.

/// The sizes of ordinary pages: they start small, so a nearly empty store
/// takes little memory, and grow with the data stored. Data larger than a
/// quarter of the largest page size gets a page of its own.
const MIN_PAGE_BYTES: usize = 16 * 1024;
const PAGE_BYTES: usize = 256 * 1024;

/// Where bytes were placed by [`Pages::alloc`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub page: u32,
    pub offset: u32,
    pub len: u32,
}

/// Packs byte strings into large pages, so each costs its length rather than
/// an allocation of its own. Freed space is reclaimed by [`Pages::compact`],
/// which rewrites the live data into new pages.
#[derive(Default)]
pub(crate) struct Pages {
    pages: Vec<Option<Page>>,
    current: Option<u32>,
    allocated: usize,
    live: usize,
}

struct Page {
    data: Box<[u8]>,
    used: usize,
    live: usize,
}

impl Pages {
    /// Copy `bytes` into a page.
    pub(crate) fn alloc(&mut self, bytes: &[u8]) -> Location {
        let page = self.page_with_room(bytes.len());
        let entry = self.pages[page as usize].as_mut().expect("page_with_room returns a live page");
        let offset = entry.used;
        entry.data[offset..offset + bytes.len()].copy_from_slice(bytes);
        entry.used += bytes.len();
        entry.live += bytes.len();
        self.live += bytes.len();
        Location {
            page,
            offset: offset as u32,
            len: bytes.len() as u32,
        }
    }

    pub(crate) fn read(&self, location: Location) -> &[u8] {
        let page = self.pages[location.page as usize].as_ref().expect("location refers to a live page");
        let start = location.offset as usize;
        &page.data[start..start + location.len as usize]
    }

    /// Mark the bytes at `location` unused. A page with nothing left in it is
    /// released straight away.
    pub(crate) fn free(&mut self, location: Location) {
        let index = location.page as usize;
        let page = self.pages[index].as_mut().expect("location refers to a live page");
        page.live -= location.len as usize;
        self.live -= location.len as usize;
        if page.live == 0 && self.current != Some(location.page) {
            self.allocated -= page.data.len();
            self.pages[index] = None;
        }
    }

    /// The bytes held by pages, used or not.
    pub(crate) fn allocated_bytes(&self) -> usize {
        self.allocated
    }

    /// The bytes holding live data.
    pub(crate) fn live_bytes(&self) -> usize {
        self.live
    }

    /// Whether enough space has been freed that compacting is worthwhile.
    pub(crate) fn should_compact(&self) -> bool {
        self.allocated - self.live > PAGE_BYTES && self.live < self.allocated / 2
    }

    /// Copy every live byte string into new pages, calling `moved` with its
    /// old and new locations, and release the old pages.
    pub(crate) fn compact(&mut self, live: impl IntoIterator<Item = Location>, mut moved: impl FnMut(Location, Location)) {
        let old = std::mem::take(self);
        for location in live {
            let new = self.alloc(old.read(location));
            moved(location, new);
        }
    }

    fn page_with_room(&mut self, len: usize) -> u32 {
        if len > PAGE_BYTES / 4 {
            return self.add_page(len);
        }
        if let Some(current) = self.current {
            let page = self.pages[current as usize].as_ref().expect("the current page is live");
            if page.data.len() - page.used >= len {
                return current;
            }
            if page.live == 0 {
                self.allocated -= page.data.len();
                self.pages[current as usize] = None;
            }
        }
        let size = (self.allocated / 4).clamp(MIN_PAGE_BYTES, PAGE_BYTES).max(len);
        let page = self.add_page(size);
        self.current = Some(page);
        page
    }

    fn add_page(&mut self, size: usize) -> u32 {
        let page = Page {
            data: vec![0u8; size].into_boxed_slice(),
            used: 0,
            live: 0,
        };
        self.allocated += size;
        if let Some(free) = self.pages.iter().position(Option::is_none) {
            self.pages[free] = Some(page);
            return free as u32;
        }
        self.pages.push(Some(page));
        (self.pages.len() - 1) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_then_read() {
        let mut pages = Pages::default();
        let a = pages.alloc(b"hello");
        let b = pages.alloc(b"world");
        assert_eq!(pages.read(a), b"hello");
        assert_eq!(pages.read(b), b"world");
        assert_eq!(pages.live_bytes(), 10);
    }

    #[test]
    fn large_data_gets_its_own_page() {
        let mut pages = Pages::default();
        let large = vec![7u8; PAGE_BYTES * 2];
        let location = pages.alloc(&large);
        assert_eq!(pages.read(location), large.as_slice());
        pages.free(location);
        assert_eq!(pages.allocated_bytes(), 0);
    }

    #[test]
    fn compact_moves_live_data() {
        let mut pages = Pages::default();
        let chunk = vec![1u8; PAGE_BYTES / 8];
        let locations: Vec<Location> = (0..64).map(|_| pages.alloc(&chunk)).collect();
        let (keep, drop): (Vec<_>, Vec<_>) = locations.iter().enumerate().partition(|(i, _)| i % 4 == 0);
        for (_, location) in drop {
            pages.free(*location);
        }
        assert!(pages.should_compact());

        let mut moved = Vec::new();
        pages.compact(keep.iter().map(|(_, l)| **l), |old, new| moved.push((old, new)));
        assert_eq!(moved.len(), 16);
        for (_, new) in moved {
            assert_eq!(pages.read(new), chunk.as_slice());
        }
        assert_eq!(pages.live_bytes(), 16 * chunk.len());
        assert!(pages.allocated_bytes() < 64 * chunk.len() / 2);
    }
}
