//! The index file: sections, then their table, then a trailer.
//!
//! A section is an array of one type, little-endian, starting on a multiple
//! of 8 bytes, so that a memory-mapped file reads it in place. The table and
//! the trailer come last, so that the index is written in one pass, without
//! seeking. A reader requires the sections it knows and skips the others: an
//! index can gain sections (geohashes, say) without breaking older readers.
//!
//! - trailer, 32 bytes: magic `addok-rs`, version u32, sections u32, table
//!   offset u64, 8 bytes reserved;
//! - table entry, 24 bytes: kind u32, element size u32, offset u64, length
//!   u64 (in bytes).

use std::fmt;
use std::io::{self, Write};
use std::ops::Range;

use bytemuck::Pod;

#[cfg(not(target_endian = "little"))]
compile_error!("the index file is little-endian, and read in place");

const MAGIC: &[u8; 8] = b"addok-rs";
/// Bumped when a reader of the previous version would misread the file.
const VERSION: u32 = 1;
const TRAILER: usize = 32;
const ENTRY: usize = 24;
const ALIGNMENT: usize = 8;

macro_rules! kinds {
    ($($kind:ident = $number:literal: $element:ty,)*) => {
        /// A section of the index file. Its number and element type are part
        /// of the format.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(super) enum Kind {
            $($kind = $number,)*
        }

        impl Kind {
            const ALL: &[Kind] = &[$(Kind::$kind,)*];

            fn element(self) -> usize {
                match self {
                    $(Kind::$kind => size_of::<$element>(),)*
                }
            }

            fn name(self) -> &'static str {
                match self {
                    $(Kind::$kind => stringify!($kind),)*
                }
            }
        }
    };
}

kinds! {
    Words = 1: u8,
    WordOffsets = 2: u32,
    PostingOffsets = 3: u32,
    PostingIds = 4: u32,
    PostingScores = 5: f64,
    PairOffsets = 6: u32,
    Pairs = 7: u32,
    NgramKeys = 8: u8,
    NgramKeyOffsets = 9: u32,
    NgramOffsets = 10: u32,
    Ngrams = 11: u32,
    FilterKeys = 12: u8,
    FilterKeyOffsets = 13: u32,
    FilterOffsets = 14: u32,
    Filters = 15: u32,
    Strings = 16: u8,
    StringOffsets = 17: u32,
    ListOffsets = 18: u32,
    ListItems = 19: u32,
    DocIds = 20: u32,
    DocKinds = 21: u32,
    DocBanIds = 22: [u8; 16],
    DocTextTags = 23: u16,
    DocTexts = 24: [u32; 8],
    DocNumbers = 25: [f64; 6],
    DocFlags = 26: u16,
    DocBanIdTexts = 27: [u32; 2],
    DocHouseNumbers = 28: u32,
    HnNumbers = 29: u32,
    HnBanIds = 30: [u8; 16],
    HnCoordinates = 31: [i32; 4],
    HnFlags = 32: u8,
    HnIdExceptions = 33: [u32; 2],
    HnCoordinateExceptionIndexes = 34: u32,
    HnCoordinateExceptions = 35: [f64; 4],
    HnBanIdTexts = 36: [u32; 2],
    NumberWritten = 37: u8,
    NumberWrittenOffsets = 38: u32,
    NumberTokens = 39: u8,
    NumberTokenOffsets = 40: u32,
    WordTable = 41: u32,
}

/// Writes sections, then their table and the trailer.
pub(super) struct Writer<W> {
    out: W,
    written: u64,
    table: Vec<(Kind, u64, u64)>,
    open: Option<(Kind, u64)>,
}

impl<W: Write> Writer<W> {
    pub(super) fn new(out: W) -> Self {
        Writer {
            out,
            written: 0,
            table: Vec::new(),
            open: None,
        }
    }

    /// A whole section.
    pub(super) fn section<T: Pod>(&mut self, kind: Kind, items: &[T]) -> io::Result<()> {
        self.begin(kind);
        self.extend(items)?;
        self.end()
    }

    /// Strings as one section of their bytes and one of their offsets, from
    /// 0 to the end, which a reader's `Strings` reads.
    pub(super) fn strings<'s>(
        &mut self,
        blob: Kind,
        offsets: Kind,
        strings: impl IntoIterator<Item = &'s str>,
    ) -> io::Result<()> {
        let mut ends = vec![0u32];
        self.begin(blob);
        for string in strings {
            self.extend(string.as_bytes())?;
            let end = ends.last().unwrap() + string.len() as u32;
            ends.push(end);
        }
        self.end()?;
        let length = u64::from(*ends.last().unwrap());
        assert_eq!(length, self.table.last().unwrap().2, "strings under 4 GiB");
        self.section(offsets, &ends)
    }

    /// A section to write in parts, with `extend` then `end`.
    pub(super) fn begin(&mut self, kind: Kind) {
        assert!(self.open.is_none(), "one section at a time");
        self.open = Some((kind, self.written));
    }

    pub(super) fn extend<T: Pod>(&mut self, items: &[T]) -> io::Result<()> {
        let (kind, _) = self.open.expect("a section begun");
        assert_eq!(size_of::<T>(), kind.element(), "{} elements", kind.name());
        self.raw(bytemuck::cast_slice(items))
    }

    pub(super) fn end(&mut self) -> io::Result<()> {
        let (kind, start) = self.open.take().expect("a section begun");
        self.table.push((kind, start, self.written - start));
        let padding = (ALIGNMENT - self.written as usize % ALIGNMENT) % ALIGNMENT;
        self.raw(&[0; ALIGNMENT][..padding])
    }

    fn raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Writes the table and the trailer, and returns each section's size in
    /// bytes, in the file's order.
    pub(super) fn finish(mut self) -> io::Result<Vec<(&'static str, u64)>> {
        assert!(self.open.is_none(), "every section ended");
        let table = self.written;
        let entries = std::mem::take(&mut self.table);
        for &(kind, offset, length) in &entries {
            let mut entry = [0u8; ENTRY];
            entry[..4].copy_from_slice(&(kind as u32).to_le_bytes());
            entry[4..8].copy_from_slice(&(kind.element() as u32).to_le_bytes());
            entry[8..16].copy_from_slice(&offset.to_le_bytes());
            entry[16..].copy_from_slice(&length.to_le_bytes());
            self.raw(&entry)?;
        }
        let mut trailer = [0u8; TRAILER];
        trailer[..8].copy_from_slice(MAGIC);
        trailer[8..12].copy_from_slice(&VERSION.to_le_bytes());
        trailer[12..16].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        trailer[16..24].copy_from_slice(&table.to_le_bytes());
        self.raw(&trailer)?;
        self.out.flush()?;
        let sizes = entries
            .iter()
            .map(|&(kind, _, length)| (kind.name(), length));
        Ok(sizes.collect())
    }
}

/// Why bytes are not an index addok-rs can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    NotAnIndex,
    /// Written by a version of the format this build does not read.
    Version(u32),
    /// Not 8-byte aligned, as a memory-mapped file or `AlignedBytes` are.
    Misaligned,
    /// A section this build requires is missing: the file was written by
    /// an older addok-rs, before the section was added, or is damaged.
    MissingSection(&'static str),
    Corrupt(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            OpenError::NotAnIndex => f.write_str("not an addok-rs index"),
            OpenError::Version(version) => {
                write!(
                    f,
                    "index format version {version}; this build reads version {VERSION}"
                )
            }
            OpenError::Misaligned => f.write_str("index bytes not aligned on 8 bytes"),
            OpenError::MissingSection(name) => write!(
                f,
                "index without a {name} section: written by an older addok-rs, or damaged"
            ),
            OpenError::Corrupt(what) => write!(f, "corrupt index: {what}"),
        }
    }
}

impl std::error::Error for OpenError {}

fn corrupt(what: String) -> OpenError {
    OpenError::Corrupt(what)
}

/// Where each section lies in an index, checked: within the file, aligned,
/// a whole number of elements of the right size.
pub(super) struct Sections {
    ranges: Vec<Option<Range<usize>>>,
}

impl Sections {
    pub(super) fn read(bytes: &[u8]) -> Result<Sections, OpenError> {
        if !(bytes.as_ptr() as usize).is_multiple_of(ALIGNMENT) {
            return Err(OpenError::Misaligned);
        }
        let Some(start) = bytes.len().checked_sub(TRAILER) else {
            return Err(OpenError::NotAnIndex);
        };
        let trailer = &bytes[start..];
        if &trailer[..8] != MAGIC {
            return Err(OpenError::NotAnIndex);
        }
        let number = |at: usize| u32::from_le_bytes(trailer[at..at + 4].try_into().unwrap());
        let version = number(8);
        if version != VERSION {
            return Err(OpenError::Version(version));
        }
        let count = number(12) as usize;
        let table = u64::from_le_bytes(trailer[16..24].try_into().unwrap());
        let table = usize::try_from(table).map_err(|_| corrupt("table offset".into()))?;
        if count
            .checked_mul(ENTRY)
            .and_then(|size| table.checked_add(size))
            != Some(start)
        {
            return Err(corrupt("table size".into()));
        }
        let mut ranges = vec![None; Kind::ALL.iter().map(|&kind| kind as usize).max().unwrap() + 1];
        for entry in bytes[table..start].as_chunks::<ENTRY>().0 {
            let kind = u32::from_le_bytes(entry[..4].try_into().unwrap());
            let element = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
            let offset = u64::from_le_bytes(entry[8..16].try_into().unwrap());
            let length = u64::from_le_bytes(entry[16..].try_into().unwrap());
            let Some(&kind) = Kind::ALL.iter().find(|&&known| known as u32 == kind) else {
                continue; // A section of a later index, which this build ignores.
            };
            let name = kind.name();
            let (offset, length) = match (usize::try_from(offset), usize::try_from(length)) {
                (Ok(offset), Ok(length)) => (offset, length),
                _ => return Err(corrupt(format!("{name} bounds"))),
            };
            if element != kind.element() || length % element != 0 {
                return Err(corrupt(format!("{name} element size")));
            }
            if !offset.is_multiple_of(ALIGNMENT)
                || offset.checked_add(length).is_none_or(|end| end > table)
            {
                return Err(corrupt(format!("{name} bounds")));
            }
            if ranges[kind as usize]
                .replace(offset..offset + length)
                .is_some()
            {
                return Err(corrupt(format!("{name} twice")));
            }
        }
        if let Some(missing) = Kind::ALL
            .iter()
            .find(|&&kind| ranges[kind as usize].is_none())
        {
            return Err(OpenError::MissingSection(missing.name()));
        }
        Ok(Sections { ranges })
    }

    /// A section's elements, from the bytes `read` checked.
    pub(super) fn array<'a, T: Pod>(&self, bytes: &'a [u8], kind: Kind) -> &'a [T] {
        debug_assert_eq!(size_of::<T>(), kind.element(), "{} elements", kind.name());
        let range = self.ranges[kind as usize].clone().expect("checked by read");
        bytemuck::cast_slice(&bytes[range])
    }
}

/// Strings read from the sections `Writer::strings` writes.
#[derive(Clone, Copy)]
pub(super) struct Strings<'a> {
    blob: &'a [u8],
    offsets: &'a [u32],
}

impl<'a> Strings<'a> {
    pub(super) fn new(blob: &'a [u8], offsets: &'a [u32]) -> Self {
        Strings { blob, offsets }
    }

    pub(super) fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub(super) fn bytes(&self, i: usize) -> &'a [u8] {
        &self.blob[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    pub(super) fn get(&self, i: usize) -> &'a str {
        std::str::from_utf8(self.bytes(i)).expect("index strings are UTF-8")
    }

    /// A string's position among strings written in byte order.
    pub(super) fn find(&self, key: &str) -> Option<usize> {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.bytes(middle).cmp(key.as_bytes()) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    /// A string's position, looked up in the strings' `hash_table`.
    pub(super) fn find_hashed(&self, table: &[u32], key: &str) -> Option<usize> {
        let mut slot = slot(table.len(), key);
        // Bounded, should a corrupt table have no free slot.
        for _ in 0..table.len() {
            match table[slot] {
                0 => return None,
                i if self.bytes(i as usize - 1) == key.as_bytes() => return Some(i as usize - 1),
                _ => slot = (slot + 1) % table.len(),
            }
        }
        None
    }
}

/// Strings by hash, for `Strings::find_hashed`: a power of two slots, at
/// least two per string, each 0 or a string's position plus 1. A string
/// takes the slot its hash names or, taken, the next free one.
pub(super) fn hash_table<'s>(strings: impl ExactSizeIterator<Item = &'s str>) -> Vec<u32> {
    let mut table = vec![0u32; (2 * strings.len()).next_power_of_two().max(2)];
    for (i, string) in (1..).zip(strings) {
        let mut slot = slot(table.len(), string);
        while table[slot] != 0 {
            slot = (slot + 1) % table.len();
        }
        table[slot] = i;
    }
    table
}

/// The slot a string's hash names in a table of `size` slots, a power of
/// two: the top bits of FxHash's steps over its bytes, in little-endian
/// words of 8, the last padded with zeros. Part of the format.
fn slot(size: usize, key: &str) -> usize {
    let mut hash = 0u64;
    for chunk in key.as_bytes().chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        hash = (hash.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    (hash >> (64 - size.trailing_zeros())) as usize
}

/// Index bytes held in memory, 8-byte aligned as a memory-mapped file is:
/// write an index into it, then open it.
#[derive(Default)]
pub struct AlignedBytes {
    words: Vec<u64>,
    len: usize,
}

impl Write for AlignedBytes {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self.len + buf.len();
        self.words.resize(end.div_ceil(8), 0);
        bytemuck::cast_slice_mut::<u64, u8>(&mut self.words)[self.len..end].copy_from_slice(buf);
        self.len = end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsRef<[u8]> for AlignedBytes {
    fn as_ref(&self) -> &[u8] {
        &bytemuck::cast_slice(&self.words)[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file of every section but `skip`, each three elements of its kind's
    /// number repeated, and, if `again`, a second `Words` section.
    fn file(skip: Option<Kind>, again: bool) -> AlignedBytes {
        let mut bytes = AlignedBytes::default();
        let mut writer = Writer::new(&mut bytes);
        let kinds = Kind::ALL.iter().filter(|&&kind| Some(kind) != skip);
        for &kind in kinds.chain(again.then_some(&Kind::Words)) {
            writer.begin(kind);
            writer.raw(&vec![kind as u8; kind.element() * 3]).unwrap();
            writer.end().unwrap();
        }
        writer.finish().unwrap();
        bytes
    }

    /// Sets the byte `from_end` bytes before the end of the file.
    fn patch(bytes: &mut AlignedBytes, from_end: usize, value: u8) {
        let len = bytes.len;
        bytemuck::cast_slice_mut::<u64, u8>(&mut bytes.words)[len - from_end] = value;
    }

    #[test]
    fn reads_back_every_section() {
        let bytes = file(None, false);
        let sections = Sections::read(bytes.as_ref()).unwrap();
        let ids: &[u32] = sections.array(bytes.as_ref(), Kind::PostingIds);
        assert_eq!(ids, [0x04040404; 3]);
        let scores: &[f64] = sections.array(bytes.as_ref(), Kind::PostingScores);
        assert_eq!(scores.len(), 3);
    }

    #[test]
    fn refuses_what_is_not_an_index() {
        let empty = AlignedBytes::default();
        assert_eq!(
            Sections::read(empty.as_ref()).err(),
            Some(OpenError::NotAnIndex)
        );
        let mut bytes = file(None, false);
        patch(&mut bytes, TRAILER, b'x');
        assert_eq!(
            Sections::read(bytes.as_ref()).err(),
            Some(OpenError::NotAnIndex)
        );
    }

    #[test]
    fn refuses_another_version_a_missing_section_or_one_twice() {
        let mut bytes = file(None, false);
        patch(&mut bytes, TRAILER - 8, 9);
        assert_eq!(
            Sections::read(bytes.as_ref()).err(),
            Some(OpenError::Version(9))
        );
        let missing = file(Some(Kind::Pairs), false);
        let error = Some(OpenError::MissingSection("Pairs"));
        assert_eq!(Sections::read(missing.as_ref()).err(), error);
        let twice = file(None, true);
        let error = Some(OpenError::Corrupt("Words twice".into()));
        assert_eq!(Sections::read(twice.as_ref()).err(), error);
    }

    #[test]
    fn skips_a_section_of_a_later_index() {
        // The second Words section renumbered 999, a kind this build ignores.
        let mut bytes = file(None, true);
        patch(&mut bytes, TRAILER + ENTRY, 0xe7);
        patch(&mut bytes, TRAILER + ENTRY - 1, 0x03);
        assert!(Sections::read(bytes.as_ref()).is_ok());
    }

    #[test]
    fn refuses_misaligned_bytes() {
        let bytes = file(None, false);
        let shifted = &bytes.as_ref()[1..];
        assert_eq!(Sections::read(shifted).err(), Some(OpenError::Misaligned));
    }

    #[test]
    fn finds_a_string_among_sorted_ones() {
        let offsets = [0u32, 2, 4, 6];
        let strings = Strings::new(b"delaru", &offsets);
        assert_eq!(strings.find("la"), Some(1));
        assert_eq!(strings.find("le"), None);
        assert_eq!(strings.find(""), None);
        assert_eq!(strings.get(2), "ru");
    }

    #[test]
    fn finds_a_string_by_its_hash() {
        let words = ["a", "de", "la", "le", "ru", "chateauneuf"];
        let mut blob = String::new();
        let mut offsets = vec![0u32];
        for word in words {
            blob.push_str(word);
            offsets.push(blob.len() as u32);
        }
        let strings = Strings::new(blob.as_bytes(), &offsets);
        let table = hash_table(words.into_iter());
        assert_eq!(table.len(), 16);
        for (i, word) in words.into_iter().enumerate() {
            assert_eq!(strings.find_hashed(&table, word), Some(i));
        }
        for absent in ["", "b", "chateauneu", "chateauneufs"] {
            assert_eq!(strings.find_hashed(&table, absent), None);
        }
        assert_eq!(hash_table([].into_iter()), [0, 0]);
    }
}
