use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::time::{Duration, SystemTime};

use crate::index::{EntryId, Index, Kind, SizeMetric, Sizes};

pub const MAX_EXPRESSION_BYTES: usize = 4096;
pub const MAX_CONDITIONS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    Yes,
    No,
    Unknown,
}

impl Match {
    fn not(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
            Self::Unknown => self,
        }
    }
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::No, _) | (_, Self::No) => Self::No,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Yes,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterError {
    TooLong,
    TooManyConditions,
    EmptyCondition,
    UnsupportedSyntax,
    InvalidSize,
    InvalidDuration,
    UnknownAttribute,
    UnknownClass,
    UnknownTag,
}

/// Optional observations, never inferred from absent metadata. Timestamps use Windows FILETIME
/// ticks (100 ns since 1601); zero and values outside SystemTime's range remain unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub created: u64,
    pub modified: u64,
    pub accessed: u64,
    pub attributes: Option<u32>,
    pub ads: Option<bool>,
}

/// Session-only class definitions. A user entry replaces a built-in entry of the same name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassTable(BTreeMap<String, Vec<String>>);
impl Default for ClassTable {
    fn default() -> Self {
        Self(
            [
                ("images", &["jpg", "jpeg", "png", "gif", "webp"][..]),
                ("audio", &["mp3", "wav", "flac"][..]),
                ("video", &["mp4", "mkv", "avi"][..]),
                ("documents", &["txt", "pdf", "docx", "md"][..]),
                ("archives", &["zip", "7z", "tar", "gz"][..]),
            ]
            .into_iter()
            .map(|(name, extensions)| {
                (
                    name.into(),
                    extensions.iter().map(|s| (*s).into()).collect(),
                )
            })
            .collect(),
        )
    }
}
impl ClassTable {
    pub fn set(&mut self, name: &str, extensions: &[&str]) -> Result<(), FilterError> {
        if !identifier(name) || extensions.is_empty() || extensions.iter().any(|s| !identifier(s)) {
            return Err(FilterError::UnknownClass);
        }
        self.0.insert(
            name.to_ascii_lowercase(),
            extensions.iter().map(|s| s.to_ascii_lowercase()).collect(),
        );
        Ok(())
    }
}
fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Four temporary bits, scoped to one index. A directory assignment is inherited by descendants;
/// an explicit zero clears inherited tags. An absent assignment at the root means untagged.
#[derive(Debug, Default)]
pub struct TagTable(HashMap<EntryId, u8>);
impl TagTable {
    pub fn set(&mut self, id: EntryId, bits: u8) -> Result<(), FilterError> {
        if bits & !15 != 0 {
            return Err(FilterError::UnknownTag);
        }
        self.0.insert(id, bits);
        Ok(())
    }
    fn get(&self, index: &Index, mut id: EntryId) -> Option<u8> {
        loop {
            let entry = index.get(id)?;
            if let Some(bits) = self.0.get(&id) {
                return Some(*bits);
            }
            match entry.parent {
                Some(parent) => id = parent,
                None => return Some(0),
            }
        }
    }
    pub(crate) fn memory_bytes(&self) -> usize {
        self.0.capacity() * (std::mem::size_of::<(EntryId, u8)>() + 1)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    includes: Vec<Mask>,
    excludes: Vec<Mask>,
    include_extensions: HashSet<Vec<u8>>,
    exclude_extensions: HashSet<Vec<u8>>,
    sizes: Vec<(SizeMetric, Comparison, u64)>,
    ages: Vec<(Age, Comparison, Duration)>,
    attributes: Vec<(Attribute, bool)>,
    tags: Vec<(u8, bool)>,
    reference_time: Option<SystemTime>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mask {
    folder: bool,
    folder_bit: u64,
    pattern: Vec<char>,
    ascii: Option<Vec<u8>>,
    suffix: Option<String>,
}
impl Mask {
    fn new(folder: bool, pattern: &str) -> Self {
        let pattern = pattern.to_ascii_lowercase();
        let suffix = pattern
            .strip_prefix('*')
            .filter(|suffix| !suffix.contains(['*', '?']))
            .map(str::to_owned);
        Self {
            folder,
            folder_bit: 0,
            ascii: pattern.is_ascii().then(|| pattern.as_bytes().to_vec()),
            pattern: pattern.chars().collect(),
            suffix,
        }
    }
    fn evaluate(&self, name: &mut Name<'_>) -> Match {
        let Some(text) = name.text else {
            return Match::Unknown;
        };
        if let Some(suffix) = &self.suffix
            && !text.starts_with('*')
        {
            return decision(
                text.get(text.len().saturating_sub(suffix.len())..)
                    .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix)),
            );
        }
        if let Some(pattern) = &self.ascii
            && text.is_ascii()
        {
            return decision(wildcard(pattern, name.ascii()));
        }
        decision(wildcard(&self.pattern, name.scalars()))
    }
}

/// Scratch belongs to one evaluation worker, not to an entry or a condition.
#[derive(Default)]
struct NameScratch {
    ascii: Vec<u8>,
    scalars: Vec<char>,
}
struct Name<'a> {
    text: Option<&'a str>,
    scratch: &'a mut NameScratch,
    ascii_ready: bool,
    scalars_ready: bool,
}
impl<'a> Name<'a> {
    fn new(name: &'a OsStr, scratch: &'a mut NameScratch) -> Self {
        Self {
            text: name.to_str(),
            scratch,
            ascii_ready: false,
            scalars_ready: false,
        }
    }
    fn ascii(&mut self) -> &[u8] {
        if !self.ascii_ready {
            self.scratch.ascii.clear();
            self.scratch
                .ascii
                .extend(self.text.unwrap().bytes().map(|b| b.to_ascii_lowercase()));
            self.ascii_ready = true;
        }
        &self.scratch.ascii
    }
    fn scalars(&mut self) -> &[char] {
        if !self.scalars_ready {
            self.scratch.scalars.clear();
            self.scratch
                .scalars
                .extend(self.text.unwrap().chars().map(|c| c.to_ascii_lowercase()));
            self.scalars_ready = true;
        }
        &self.scratch.scalars
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison {
    Greater,
    Less,
}
impl Comparison {
    fn test<T: PartialOrd>(&self, value: T, threshold: T) -> Match {
        decision(match self {
            Self::Greater => value > threshold,
            Self::Less => value < threshold,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Age {
    Created,
    Modified,
    Accessed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attribute {
    File,
    Directory,
    Flag(u32),
    Ads,
}
struct Input<'a> {
    name: &'a OsStr,
    sizes: Sizes,
    kind: Kind,
    metadata: Metadata,
    tags: Option<u8>,
}

impl Filter {
    pub fn parse(expression: &str) -> Result<Self, FilterError> {
        Self::parse_with_classes(expression, &ClassTable::default(), SystemTime::now())
    }
    /// The reference instant is fixed for this expression, so cached age decisions do not drift
    /// between frames. Reapply the expression to evaluate ages at another instant.
    pub fn parse_with_classes(
        expression: &str,
        classes: &ClassTable,
        now: SystemTime,
    ) -> Result<Self, FilterError> {
        if expression.len() > MAX_EXPRESSION_BYTES {
            return Err(FilterError::TooLong);
        }
        if expression.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut filter = Self::default();
        for (position, condition) in expression.split(';').enumerate() {
            if position >= MAX_CONDITIONS {
                return Err(FilterError::TooManyConditions);
            }
            let condition = condition.trim();
            if condition.is_empty() {
                return Err(FilterError::EmptyCondition);
            }
            let (positive, body) = if condition.starts_with("-:") {
                (false, &condition[1..])
            } else if condition.starts_with("+:") {
                (true, &condition[1..])
            } else {
                (true, condition)
            };
            if let Some(name) = body.strip_prefix(":attr:") {
                let attribute = match name.to_ascii_lowercase().as_str() {
                    "file" => Attribute::File,
                    "dir" => Attribute::Directory,
                    "system" => Attribute::Flag(4),
                    "hidden" => Attribute::Flag(2),
                    "compressed" => Attribute::Flag(0x800),
                    "encrypted" => Attribute::Flag(0x4000),
                    "offline" => Attribute::Flag(0x1000),
                    "sparse" => Attribute::Flag(0x200),
                    "ads" => Attribute::Ads,
                    _ => return Err(FilterError::UnknownAttribute),
                };
                filter.attributes.push((attribute, positive));
            } else if let Some(name) = body.strip_prefix(":class:") {
                let extensions = classes
                    .0
                    .get(&name.to_ascii_lowercase())
                    .ok_or(FilterError::UnknownClass)?;
                let target = if positive {
                    &mut filter.include_extensions
                } else {
                    &mut filter.exclude_extensions
                };
                target.extend(
                    extensions
                        .iter()
                        .map(|extension| extension.as_bytes().to_vec()),
                );
            } else if let Some(tag) = body.strip_prefix(':') {
                let bits = match tag.to_ascii_lowercase().as_str() {
                    "1" | "red" => 1,
                    "2" | "yellow" => 2,
                    "3" | "green" => 4,
                    "4" | "blue" => 8,
                    "tag:" => 15,
                    _ => return Err(FilterError::UnknownTag),
                };
                filter.tags.push((bits, positive));
            } else if body != condition {
                return Err(FilterError::UnsupportedSyntax);
            } else if let Some((prefix, comparison, number)) = comparison(condition) {
                match prefix {
                    "" | "s" | "d" => filter.sizes.push((
                        if prefix == "d" {
                            SizeMetric::Allocated
                        } else {
                            SizeMetric::Logical
                        },
                        comparison,
                        parse_size(number)?,
                    )),
                    "c" | "m" | "a" => {
                        let age = match prefix {
                            "c" => Age::Created,
                            "m" => Age::Modified,
                            _ => Age::Accessed,
                        };
                        filter.ages.push((age, comparison, parse_duration(number)?));
                        filter.reference_time = Some(now);
                    }
                    _ => return Err(FilterError::UnsupportedSyntax),
                }
            } else {
                let (excluded, pattern) = match condition.strip_prefix('|') {
                    Some(pattern) => (true, pattern),
                    None => (false, condition),
                };
                // One leading backslash is the folder operator, not a path or a UNC prefix.
                let (folder, pattern) = match pattern.strip_prefix('\\') {
                    Some(pattern) => (true, pattern),
                    None => (false, pattern),
                };
                if pattern.is_empty()
                    || pattern.chars().any(|c| {
                        c.is_control() || matches!(c, '\\' | '/' | ':' | '|' | '<' | '>' | '"')
                    })
                {
                    return Err(FilterError::UnsupportedSyntax);
                }
                let mask = Mask::new(folder, pattern);
                if excluded {
                    filter.excludes.push(mask);
                } else {
                    filter.includes.push(mask);
                }
            }
        }
        for (bit, mask) in filter
            .includes
            .iter_mut()
            .chain(&mut filter.excludes)
            .filter(|mask| mask.folder)
            .enumerate()
        {
            mask.folder_bit = 1_u64 << bit;
        }
        Ok(filter)
    }

    /// Compatible name/logical-size boundary. All supplemental metadata is unavailable here.
    pub fn evaluate(&self, name: &OsStr, logical_bytes: Option<u64>) -> Match {
        self.evaluate_input(
            Input {
                name,
                sizes: Sizes {
                    logical: logical_bytes,
                    allocated: None,
                },
                kind: Kind::File,
                metadata: Metadata::default(),
                tags: None,
            },
            |_| Match::Unknown,
            &mut NameScratch::default(),
        )
    }

    /// Evaluates listed metadata and native folder components, without querying the filesystem.
    pub fn evaluate_entry(&self, index: &Index, id: EntryId) -> Match {
        self.evaluate_entry_scratch(index, id, &mut NameScratch::default(), None)
    }
    fn evaluate_entry_scratch(
        &self,
        index: &Index,
        id: EntryId,
        scratch: &mut NameScratch,
        contexts: Option<&HashMap<EntryId, DirectoryContext>>,
    ) -> Match {
        let Some(entry) = index.get(id) else {
            return Match::Unknown;
        };
        let context = contexts.and_then(|contexts| {
            contexts.get(&if entry.kind == Kind::Directory {
                id
            } else {
                entry.parent?
            })
        });
        self.evaluate_input(
            Input {
                name: entry.name,
                sizes: entry.sizes,
                kind: entry.kind,
                metadata: if self.ages.is_empty() && self.attributes.is_empty() {
                    Metadata::default()
                } else {
                    index.filter_metadata(id)
                },
                tags: if self.tags.is_empty() {
                    None
                } else {
                    context
                        .map(|context| index.tags.0.get(&id).copied().unwrap_or(context.tags))
                        .or_else(|| index.tags.get(index, id))
                },
            },
            |mask| {
                if let Some(context) = context {
                    return context.decision(mask);
                }
                let mut next = if entry.kind == Kind::Directory {
                    Some(id)
                } else {
                    entry.parent
                };
                let mut found = Match::No;
                while let Some(id) = next {
                    let parent = index.get(id).expect("index ancestor");
                    found = combine(
                        found,
                        mask.evaluate(&mut Name::new(parent.name, &mut NameScratch::default())),
                    );
                    if found == Match::Yes {
                        break;
                    }
                    next = parent.parent;
                }
                found
            },
            scratch,
        )
    }
    fn evaluate_input(
        &self,
        input: Input<'_>,
        folder: impl Fn(&Mask) -> Match,
        scratch: &mut NameScratch,
    ) -> Match {
        let Input {
            name,
            sizes,
            kind,
            metadata,
            tags,
        } = input;
        let mut result = Match::Yes;
        for (attribute, required) in &self.attributes {
            let value = match attribute {
                Attribute::File => decision(kind == Kind::File),
                Attribute::Directory => decision(kind == Kind::Directory),
                Attribute::Flag(flag) => metadata
                    .attributes
                    .map_or(Match::Unknown, |bits| decision(bits & flag != 0)),
                Attribute::Ads => metadata.ads.map_or(Match::Unknown, decision),
            };
            result = result.and(if *required { value } else { value.not() });
            if result == Match::No {
                return result;
            }
        }
        for (bits, required) in &self.tags {
            if !required {
                result = result.and(tags.map_or(Match::Unknown, |tags| decision(tags & bits == 0)));
                if result == Match::No {
                    return result;
                }
            }
        }
        for (metric, comparison, threshold) in &self.sizes {
            result = result.and(
                metric
                    .of(sizes)
                    .map_or(Match::Unknown, |bytes| comparison.test(bytes, *threshold)),
            );
            if result == Match::No {
                return result;
            }
        }
        let mut name = Name::new(name, scratch);
        let mut included = if self.includes.is_empty()
            && self.include_extensions.is_empty()
            && !self.tags.iter().any(|(_, required)| *required)
        {
            Match::Yes
        } else {
            Match::No
        };
        for (bits, required) in &self.tags {
            if *required {
                included = combine(
                    included,
                    tags.map_or(Match::Unknown, |tags| decision(tags & bits != 0)),
                );
            }
        }
        if included != Match::Yes && !self.include_extensions.is_empty() {
            included = combine(
                included,
                match_extension(&self.include_extensions, &mut name),
            );
        }
        for mask in &self.includes {
            if included == Match::Yes {
                break;
            }
            included = combine(
                included,
                if mask.folder {
                    folder(mask)
                } else {
                    mask.evaluate(&mut name)
                },
            );
        }
        result = result.and(included);
        if result == Match::No {
            return result;
        }
        if !self.exclude_extensions.is_empty() {
            result = result.and(match_extension(&self.exclude_extensions, &mut name).not());
            if result == Match::No {
                return result;
            }
        }
        for mask in &self.excludes {
            result = result.and(
                if mask.folder {
                    folder(mask)
                } else {
                    mask.evaluate(&mut name)
                }
                .not(),
            );
            if result == Match::No {
                return result;
            }
        }
        for (age, comparison, threshold) in &self.ages {
            let ticks = match age {
                Age::Created => metadata.created,
                Age::Modified => metadata.modified,
                Age::Accessed => metadata.accessed,
            };
            let age =
                timestamp(ticks).and_then(|time| self.reference_time?.duration_since(time).ok());
            result = result.and(age.map_or(Match::Unknown, |age| comparison.test(age, *threshold)));
            if result == Match::No {
                return result;
            }
        }
        result
    }
    fn needs_directories(&self) -> bool {
        !self.tags.is_empty()
            || self
                .includes
                .iter()
                .chain(&self.excludes)
                .any(|mask| mask.folder)
    }
    pub fn matches(&self, index: &Index) -> FilterMatches {
        let mut storage = index
            .filter_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if storage.as_ref().is_none_or(|cache| cache.filter != *self) {
            *storage = Some(MatchCache::build(index, self.clone()));
        }
        let cache = storage.as_ref().expect("initialized filter matches");
        FilterMatches {
            matches: cache.matches.to_vec(),
            matched_files: cache.matched_files,
            unknown_files: cache.unknown_files,
        }
    }
}
fn match_extension(extensions: &HashSet<Vec<u8>>, name: &mut Name<'_>) -> Match {
    if name.text.is_none() {
        return Match::Unknown;
    }
    let bytes = name.ascii();
    decision(
        bytes
            .iter()
            .rposition(|byte| *byte == b'.')
            .is_some_and(|dot| {
                // Preserve the scalar matcher's literal-first behavior on wildcard-shaped native names.
                (bytes.first() != Some(&b'*') || dot == 1) && extensions.contains(&bytes[dot + 1..])
            }),
    )
}
fn decision(value: bool) -> Match {
    if value { Match::Yes } else { Match::No }
}
fn comparison(text: &str) -> Option<(&str, Comparison, &str)> {
    let position = text.find(['<', '>'])?;
    Some((
        &text[..position],
        if text.as_bytes()[position] == b'>' {
            Comparison::Greater
        } else {
            Comparison::Less
        },
        &text[position + 1..],
    ))
}
fn timestamp(ticks: u64) -> Option<SystemTime> {
    if ticks == 0 {
        return None;
    }
    let since_1601 =
        Duration::from_nanos((ticks % 10_000_000) * 100) + Duration::from_secs(ticks / 10_000_000);
    SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(11_644_473_600))?
        .checked_add(since_1601)
}
fn parse_duration(text: &str) -> Result<Duration, FilterError> {
    let (number, unit) = number_unit(text).map_err(|_| FilterError::InvalidDuration)?;
    let multiplier = match unit.as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604800,
        _ => return Err(FilterError::InvalidDuration),
    };
    number
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .ok_or(FilterError::InvalidDuration)
}
fn number_unit(text: &str) -> Result<(u64, String), FilterError> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(FilterError::InvalidSize);
    }
    Ok((
        text[..digits]
            .parse()
            .map_err(|_| FilterError::InvalidSize)?,
        text[digits..].to_ascii_lowercase(),
    ))
}
fn parse_size(text: &str) -> Result<u64, FilterError> {
    let (number, unit) = number_unit(text)?;
    let multiplier = match unit.as_str() {
        "b" => 1,
        "k" | "kb" => 1024,
        "m" | "mb" => 1024_u64.pow(2),
        "g" | "gb" => 1024_u64.pow(3),
        "t" | "tb" => 1024_u64.pow(4),
        _ => return Err(FilterError::InvalidSize),
    };
    number
        .checked_mul(multiplier)
        .ok_or(FilterError::InvalidSize)
}

#[derive(Debug)]
pub struct FilterMatches {
    pub matches: Vec<Match>,
    pub matched_files: usize,
    pub unknown_files: usize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilterTotals {
    pub matched_bytes: Option<u64>,
    pub undecided_bytes: Option<u64>,
    pub unknown_size_files: usize,
}
impl FilterMatches {
    /// Per-name apparent weights, including corrections only at their original first charge.
    /// Unknown decisions are retained separately and are never added to matched bytes.
    pub fn totals(&self, index: &Index, metric: SizeMetric) -> FilterTotals {
        let mut totals = FilterTotals {
            matched_bytes: Some(0),
            undecided_bytes: Some(0),
            unknown_size_files: 0,
        };
        let mut seen = std::collections::HashSet::new();
        let corrected = index.has_authoritative_sizes();
        for (id, entry) in index.entries() {
            if entry.kind != Kind::File {
                continue;
            }
            let size = metric.of(entry.sizes).map(|listed| {
                if !corrected || entry.identity.is_none_or(|identity| seen.insert(identity)) {
                    index.effective_size(&entry, metric).expect("listed size")
                } else {
                    listed
                }
            });
            let value = self
                .matches
                .get(id.position())
                .copied()
                .unwrap_or(Match::Unknown);
            if value == Match::No {
                continue;
            }
            let Some(size) = size else {
                totals.unknown_size_files += 1;
                continue;
            };
            let sum = if value == Match::Yes {
                &mut totals.matched_bytes
            } else {
                &mut totals.undecided_bytes
            };
            *sum = sum.and_then(|sum| sum.checked_add(size));
        }
        totals
    }
}
#[derive(Debug)]
pub(crate) struct MatchCache {
    filter: Filter,
    matches: DecisionBits,
    matched_files: usize,
    unknown_files: usize,
    directories: HashMap<EntryId, DirectoryContext>,
}
/// Two disjoint bits encode Yes/Unknown; neither bit means No. Public results remain compatible.
#[derive(Debug, PartialEq, Eq)]
struct DecisionBits {
    yes: Vec<u64>,
    unknown: Vec<u64>,
    len: usize,
}
impl DecisionBits {
    fn new(len: usize) -> Self {
        let mut yes = Vec::with_capacity((len + 4096).div_ceil(64));
        let mut unknown = Vec::with_capacity((len + 4096).div_ceil(64));
        yes.resize(len.div_ceil(64), 0);
        unknown.resize(len.div_ceil(64), 0);
        Self { yes, unknown, len }
    }
    fn get(&self, position: usize) -> Match {
        let bit = 1_u64 << (position % 64);
        if self.yes[position / 64] & bit != 0 {
            Match::Yes
        } else if self.unknown[position / 64] & bit != 0 {
            Match::Unknown
        } else {
            Match::No
        }
    }
    fn set(&mut self, position: usize, value: Match) {
        let bit = 1_u64 << (position % 64);
        let word = position / 64;
        self.yes[word] &= !bit;
        self.unknown[word] &= !bit;
        match value {
            Match::Yes => self.yes[word] |= bit,
            Match::Unknown => self.unknown[word] |= bit,
            Match::No => {}
        }
    }
    fn push(&mut self, value: Match) {
        if self.len.is_multiple_of(64) {
            if self.yes.len() == self.yes.capacity() {
                self.yes.reserve_exact(64);
            }
            if self.unknown.len() == self.unknown.capacity() {
                self.unknown.reserve_exact(64);
            }
            self.yes.push(0);
            self.unknown.push(0);
        }
        self.set(self.len, value);
        self.len += 1;
    }
    fn to_vec(&self) -> Vec<Match> {
        (0..self.len).map(|position| self.get(position)).collect()
    }
    fn memory_bytes(&self) -> usize {
        (self.yes.capacity() + self.unknown.capacity()) * std::mem::size_of::<u64>()
    }
}
#[derive(Clone, Copy, Debug, Default)]
struct DirectoryContext {
    yes: u64,
    unknown: u64,
    tags: u8,
}
impl DirectoryContext {
    fn decision(&self, mask: &Mask) -> Match {
        if self.yes & mask.folder_bit != 0 {
            Match::Yes
        } else if self.unknown & mask.folder_bit != 0 {
            Match::Unknown
        } else {
            Match::No
        }
    }
}
impl MatchCache {
    fn build(index: &Index, filter: Filter) -> Self {
        // Match the scanner's automatic four-worker ceiling, without exceeding the host.
        let workers = if index.len() < 65536 {
            1
        } else {
            std::thread::available_parallelism().map_or(1, |count| count.get().min(4))
        };
        Self::build_with_workers(index, filter, workers)
    }
    fn build_with_workers(index: &Index, filter: Filter, workers: usize) -> Self {
        let mut cache = Self {
            filter,
            matches: DecisionBits::new(index.len()),
            matched_files: 0,
            unknown_files: 0,
            directories: HashMap::new(),
        };
        if cache.filter.needs_directories() {
            let mut scratch = NameScratch::default();
            for (id, entry) in index.entries() {
                if entry.kind == Kind::Directory {
                    cache.append_directory(index, id, &mut scratch);
                }
            }
        }
        let filter = &cache.filter;
        let contexts = (!cache.directories.is_empty()).then_some(&cache.directories);
        let evaluate = |start_word: usize, yes: &mut [u64], unknown: &mut [u64]| {
            let mut scratch = NameScratch::default();
            let mut counts = (0, 0);
            for (word, (yes, unknown)) in yes.iter_mut().zip(unknown).enumerate() {
                for bit in 0..64 {
                    let position = (start_word + word) * 64 + bit;
                    if position >= index.len() {
                        break;
                    }
                    let id = EntryId::at(position).expect("index slot");
                    if index.get(id).expect("index entry").kind == Kind::File {
                        match filter.evaluate_entry_scratch(index, id, &mut scratch, contexts) {
                            Match::Yes => {
                                *yes |= 1_u64 << bit;
                                counts.0 += 1;
                            }
                            Match::Unknown => {
                                *unknown |= 1_u64 << bit;
                                counts.1 += 1;
                            }
                            Match::No => {}
                        }
                    }
                }
            }
            counts
        };
        let counts = if workers <= 1 {
            evaluate(0, &mut cache.matches.yes, &mut cache.matches.unknown)
        } else {
            std::thread::scope(|scope| {
                let chunk_size = cache.matches.yes.len().div_ceil(workers);
                let handles: Vec<_> = cache
                    .matches
                    .yes
                    .chunks_mut(chunk_size)
                    .zip(cache.matches.unknown.chunks_mut(chunk_size))
                    .enumerate()
                    .map(|(chunk, (yes, unknown))| {
                        let evaluate = &evaluate;
                        scope.spawn(move || evaluate(chunk * chunk_size, yes, unknown))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("filter worker"))
                    .fold((0, 0), |sum, count| (sum.0 + count.0, sum.1 + count.1))
            })
        };
        cache.matched_files = counts.0;
        cache.unknown_files = counts.1;
        for (id, entry) in index.entries().rev() {
            if let Some(parent) = entry.parent {
                cache.matches.set(
                    parent.position(),
                    combine(
                        cache.matches.get(parent.position()),
                        cache.matches.get(id.position()),
                    ),
                );
            }
        }
        cache
    }
    fn append_directory(&mut self, index: &Index, id: EntryId, scratch: &mut NameScratch) {
        let entry = index.get(id).expect("directory entry");
        let mut context = entry
            .parent
            .and_then(|parent| self.directories.get(&parent).copied())
            .unwrap_or_default();
        context.tags = index.tags.0.get(&id).copied().unwrap_or(context.tags);
        let mut name = Name::new(entry.name, scratch);
        for mask in self
            .filter
            .includes
            .iter()
            .chain(&self.filter.excludes)
            .filter(|mask| mask.folder)
        {
            if context.yes & mask.folder_bit == 0 {
                match mask.evaluate(&mut name) {
                    Match::Yes => {
                        context.yes |= mask.folder_bit;
                        context.unknown &= !mask.folder_bit;
                    }
                    Match::Unknown => context.unknown |= mask.folder_bit,
                    Match::No => {}
                }
            }
        }
        self.directories.insert(id, context);
    }
    pub(crate) fn append(&mut self, index: &Index, id: EntryId) {
        let entry = index.get(id).expect("new filter entry");
        let mut scratch = NameScratch::default();
        if entry.kind == Kind::Directory && self.filter.needs_directories() {
            self.append_directory(index, id, &mut scratch);
        }
        let value = if entry.kind == Kind::File {
            self.filter.evaluate_entry_scratch(
                index,
                id,
                &mut scratch,
                (!self.directories.is_empty()).then_some(&self.directories),
            )
        } else {
            Match::No
        };
        self.matched_files += usize::from(value == Match::Yes);
        self.unknown_files += usize::from(value == Match::Unknown);
        self.matches.push(value);
        let mut next = entry.parent;
        while let Some(parent) = next {
            let previous = self.matches.get(parent.position());
            let combined = combine(previous, value);
            if previous == combined {
                break;
            }
            self.matches.set(parent.position(), combined);
            next = index.get(parent).expect("existing ancestor").parent;
        }
    }
    pub(crate) fn memory_bytes(&self) -> usize {
        self.matches.memory_bytes()
            + (self.filter.includes.capacity() + self.filter.excludes.capacity())
                * std::mem::size_of::<Mask>()
            + self
                .filter
                .includes
                .iter()
                .chain(&self.filter.excludes)
                .map(|m| {
                    m.pattern.capacity() * std::mem::size_of::<char>()
                        + m.ascii.as_ref().map_or(0, Vec::capacity)
                        + m.suffix.as_ref().map_or(0, String::capacity)
                })
                .sum::<usize>()
            + self.filter.sizes.capacity() * std::mem::size_of::<(SizeMetric, Comparison, u64)>()
            + self.filter.ages.capacity() * std::mem::size_of::<(Age, Comparison, Duration)>()
            + self.filter.attributes.capacity() * std::mem::size_of::<(Attribute, bool)>()
            + self.filter.tags.capacity() * std::mem::size_of::<(u8, bool)>()
            + crate::index::hash_bytes::<(EntryId, DirectoryContext)>(self.directories.capacity())
            + [
                &self.filter.include_extensions,
                &self.filter.exclude_extensions,
            ]
            .into_iter()
            .map(|extensions| {
                crate::index::hash_bytes::<Vec<u8>>(extensions.capacity())
                    + extensions.iter().map(Vec::capacity).sum::<usize>()
            })
            .sum::<usize>()
    }
}
fn combine(left: Match, right: Match) -> Match {
    match (left, right) {
        (Match::Yes, _) | (_, Match::Yes) => Match::Yes,
        (Match::Unknown, _) | (_, Match::Unknown) => Match::Unknown,
        _ => Match::No,
    }
}
fn wildcard<T: PartialEq + From<u8>>(pattern: &[T], name: &[T]) -> bool {
    let (mut pattern_pos, mut name_pos) = (0, 0);
    let mut star = None;
    let mut retry_at = 0;
    while name_pos < name.len() {
        if pattern.get(pattern_pos) == Some(&T::from(b'?'))
            || pattern.get(pattern_pos) == name.get(name_pos)
        {
            pattern_pos += 1;
            name_pos += 1;
        } else if pattern.get(pattern_pos) == Some(&T::from(b'*')) {
            star = Some(pattern_pos);
            pattern_pos += 1;
            retry_at = name_pos;
        } else if let Some(star_pos) = star {
            retry_at += 1;
            name_pos = retry_at;
            pattern_pos = star_pos + 1;
        } else {
            return false;
        }
    }
    while pattern.get(pattern_pos) == Some(&T::from(b'*')) {
        pattern_pos += 1;
    }
    pattern_pos == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::State;

    #[test]
    fn compiled_masks_agree_with_scalar_oracle_on_adversarial_names() {
        fn naive(pattern: &str, name: &str) -> bool {
            let name: Vec<_> = name.chars().map(|c| c.to_ascii_lowercase()).collect();
            let mut row = vec![false; name.len() + 1];
            row[0] = true;
            for token in pattern.chars().map(|c| c.to_ascii_lowercase()) {
                let mut previous = row[0];
                row[0] &= token == '*';
                for (position, character) in name.iter().enumerate() {
                    let old = row[position + 1];
                    row[position + 1] = if token == '*' {
                        old || row[position]
                    } else {
                        previous && (token == '?' || token == *character)
                    };
                    previous = old;
                }
            }
            row[name.len()]
        }
        let mut names = vec![String::new()];
        let mut level = vec![String::new()];
        for _ in 0..5 {
            level = level
                .iter()
                .flat_map(|prefix| ['a', 'B', 'é', '😀', '.'].map(|c| format!("{prefix}{c}")))
                .collect();
            names.extend(level.iter().cloned());
        }
        names.extend([
            "A".repeat(255),
            format!("{}b.bin", "a".repeat(4096)),
            "😀.JPG".into(),
            "É.jpg".into(),
            "é.JPG".into(),
        ]);
        for (pattern, name, expected) in [
            ("*", "**", Match::No),
            ("*.jpg", "**.jpg", Match::No),
            ("*.jpg", "*.jpg", Match::Yes),
        ] {
            assert_eq!(
                Filter::parse(pattern)
                    .unwrap()
                    .evaluate(OsStr::new(name), None),
                expected
            );
        }
        assert_eq!(
            Filter::parse(":class:images")
                .unwrap()
                .evaluate(OsStr::new("**.jpg"), None),
            Match::No
        );
        let mut scratch = NameScratch::default();
        for pattern in [
            "*",
            "?",
            "*.*",
            "*.jpg",
            "*.é",
            "*é",
            "*😀",
            "a*B",
            "*a*a*a*b",
            "a?*?b",
            "**a**?*",
            "??",
            "é*",
            "É*",
            "*😀?",
            "*a?b*",
            "*.bin",
            "a*a*a*a*a*a*a*a*a*b",
        ] {
            let filter = Filter::parse(pattern).unwrap();
            for name in &names {
                assert_eq!(
                    filter.includes[0].evaluate(&mut Name::new(OsStr::new(name), &mut scratch)),
                    decision(naive(pattern, name)),
                    "{pattern:?} {name:?}"
                );
            }
        }
    }

    #[test]
    fn parallel_and_sequential_decisions_equal_direct_evaluation_on_fixed_tree() {
        let mut index = Index::new("root".into()).unwrap();
        let root = index.root();
        let mut directories = vec![root];
        for number in 0..32 {
            let parent = directories[number / 2];
            let id = index
                .insert(
                    parent,
                    format!("folder-{number}").into(),
                    Kind::Directory,
                    Sizes::default(),
                    None,
                    State::Complete,
                )
                .unwrap();
            directories.push(id);
        }
        index.set_tags(directories[1], 1).unwrap();
        index.set_tags(directories[3], 0).unwrap();
        for number in 0..4096 {
            let name = match number % 4 {
                0 => format!("PHOTO-{number}.JPG"),
                1 => format!("😀-{number}.bin"),
                2 => format!("report-{number}.pdf"),
                _ => format!("item-{number}"),
            };
            let id = index
                .insert(
                    directories[number % directories.len()],
                    name.into(),
                    Kind::File,
                    Sizes {
                        logical: (number % 7 != 0).then_some(number as u64),
                        allocated: Some(number as u64 * 2),
                    },
                    None,
                    State::Complete,
                )
                .unwrap();
            if number % 3 == 0 {
                index
                    .set_filter_metadata(
                        id,
                        Metadata {
                            attributes: Some((number % 8) as u32),
                            modified: 116_444_736_000_000_000,
                            ..Metadata::default()
                        },
                    )
                    .unwrap();
            }
            if number % 11 == 0 {
                index.set_tags(id, 8).unwrap();
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let directory = index
                .insert(
                    root,
                    std::ffi::OsString::from_wide(&[0xd800]),
                    Kind::Directory,
                    Sizes::default(),
                    None,
                    State::Complete,
                )
                .unwrap();
            index
                .insert(
                    directory,
                    "native.bin".into(),
                    Kind::File,
                    Sizes::default(),
                    None,
                    State::Complete,
                )
                .unwrap();
            index
                .insert(
                    root,
                    std::ffi::OsString::from_wide(&[0xdfff]),
                    Kind::File,
                    Sizes::default(),
                    None,
                    State::Complete,
                )
                .unwrap();
        }
        for expression in [
            "",
            "*.bin;s>32b",
            r"\folder-1;*.jpg;|report*",
            r"|\folder-1",
            r"\missing;:blue;-:red",
            ":class:images;:red;d<500b",
            "-:class:documents;-:attr:hidden;m<2w",
            ":attr:dir",
            "?;s<10b",
            r"\*;s>0b",
        ] {
            let filter = Filter::parse_with_classes(
                expression,
                &ClassTable::default(),
                SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            )
            .unwrap();
            let sequential = MatchCache::build_with_workers(&index, filter.clone(), 1);
            let parallel = MatchCache::build_with_workers(&index, filter.clone(), 4);
            assert_eq!(sequential.matches, parallel.matches, "{expression}");
            assert_eq!(
                (sequential.matched_files, sequential.unknown_files),
                (parallel.matched_files, parallel.unknown_files)
            );
            let mut direct: Vec<_> = index
                .entries()
                .map(|(id, entry)| {
                    if entry.kind == Kind::File {
                        filter.evaluate_entry(&index, id)
                    } else {
                        Match::No
                    }
                })
                .collect();
            for (id, entry) in index.entries().rev() {
                if let Some(parent) = entry.parent {
                    direct[parent.position()] =
                        combine(direct[parent.position()], direct[id.position()]);
                }
            }
            assert_eq!(parallel.matches.to_vec(), direct, "{expression}");
        }
    }
}
