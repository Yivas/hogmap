use std::ffi::OsStr;

use crate::index::{EntryId, Index, Kind};

pub const MAX_EXPRESSION_BYTES: usize = 4096;
pub const MAX_CONDITIONS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    Yes,
    No,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterError {
    TooLong,
    TooManyConditions,
    EmptyCondition,
    UnsupportedSyntax,
    InvalidSize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    includes: Vec<Vec<char>>,
    excludes: Vec<Vec<char>>,
    sizes: Vec<(Comparison, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Comparison {
    Greater,
    Less,
}

impl Filter {
    /// Matches listed logical sizes and propagates file matches to directory context.
    /// One parsed filter is cached per snapshot; insertions update only the new entry/ancestors.
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
            matches: cache.matches.clone(),
            matched_files: cache.matched_files,
            unknown_files: cache.unknown_files,
        }
    }

    /// Parses the documented subset without modifying a previously active filter.
    pub fn parse(expression: &str) -> Result<Self, FilterError> {
        if expression.len() > MAX_EXPRESSION_BYTES {
            return Err(FilterError::TooLong);
        }
        if expression.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut filter = Self::default();
        for (index, condition) in expression.split(';').enumerate() {
            if index >= MAX_CONDITIONS {
                return Err(FilterError::TooManyConditions);
            }
            let condition = condition.trim();
            if condition.is_empty() {
                return Err(FilterError::EmptyCondition);
            }
            if let Some(number) = condition.strip_prefix('>') {
                filter
                    .sizes
                    .push((Comparison::Greater, parse_size(number)?));
            } else if let Some(number) = condition.strip_prefix('<') {
                filter.sizes.push((Comparison::Less, parse_size(number)?));
            } else {
                let (excluded, pattern) = match condition.strip_prefix('|') {
                    Some(pattern) => (true, pattern),
                    None => (false, condition),
                };
                if pattern.is_empty()
                    || pattern.chars().any(|c| {
                        c.is_control() || matches!(c, '\\' | '/' | ':' | '|' | '<' | '>' | '"')
                    })
                {
                    return Err(FilterError::UnsupportedSyntax);
                }
                let pattern = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
                if excluded {
                    filter.excludes.push(pattern);
                } else {
                    filter.includes.push(pattern);
                }
            }
        }
        Ok(filter)
    }

    /// Matches a file name and its logical size; unknown metadata stays unknown.
    pub fn evaluate(&self, name: &OsStr, logical_bytes: Option<u64>) -> Match {
        let mut unknown = false;
        if !self.includes.is_empty() || !self.excludes.is_empty() {
            match name.to_str() {
                Some(name) => {
                    let name: Vec<_> = name.chars().map(|c| c.to_ascii_lowercase()).collect();
                    if !self.includes.is_empty()
                        && !self.includes.iter().any(|pattern| wildcard(pattern, &name))
                    {
                        return Match::No;
                    }
                    if self.excludes.iter().any(|pattern| wildcard(pattern, &name)) {
                        return Match::No;
                    }
                }
                None => unknown = true,
            }
        }
        for (comparison, threshold) in &self.sizes {
            match logical_bytes {
                Some(bytes) => {
                    let matches = match comparison {
                        Comparison::Greater => bytes > *threshold,
                        Comparison::Less => bytes < *threshold,
                    };
                    if !matches {
                        return Match::No;
                    }
                }
                None => unknown = true,
            }
        }
        if unknown { Match::Unknown } else { Match::Yes }
    }
}

#[derive(Debug)]
pub struct FilterMatches {
    pub matches: Vec<Match>,
    pub matched_files: usize,
    pub unknown_files: usize,
}

#[derive(Debug)]
pub(crate) struct MatchCache {
    filter: Filter,
    matches: Vec<Match>,
    matched_files: usize,
    unknown_files: usize,
}
impl MatchCache {
    fn build(index: &Index, filter: Filter) -> Self {
        let mut cache = Self {
            filter,
            matches: Vec::with_capacity(index.len() + 4096),
            matched_files: 0,
            unknown_files: 0,
        };
        for (_, entry) in index.entries() {
            let value = if entry.kind == Kind::File {
                let value = cache.filter.evaluate(entry.name, entry.sizes.logical);
                cache.matched_files += usize::from(value == Match::Yes);
                cache.unknown_files += usize::from(value == Match::Unknown);
                value
            } else {
                Match::No
            };
            cache.matches.push(value);
        }
        for (id, entry) in index.entries().rev() {
            if let Some(parent) = entry.parent {
                cache.matches[parent.position()] = combine(
                    cache.matches[parent.position()],
                    cache.matches[id.position()],
                );
            }
        }
        cache
    }
    pub(crate) fn append(&mut self, index: &Index, id: EntryId) {
        let entry = index.get(id).expect("new filter entry");
        let value = if entry.kind == Kind::File {
            let value = self.filter.evaluate(entry.name, entry.sizes.logical);
            self.matched_files += usize::from(value == Match::Yes);
            self.unknown_files += usize::from(value == Match::Unknown);
            value
        } else {
            Match::No
        };
        if self.matches.len() == self.matches.capacity() {
            self.matches.reserve_exact(4096);
        }
        self.matches.push(value);
        let mut next = entry.parent;
        while let Some(parent) = next {
            let previous = self.matches[parent.position()];
            let combined = combine(previous, value);
            if previous == combined {
                break;
            }
            self.matches[parent.position()] = combined;
            next = index.get(parent).expect("existing filter ancestor").parent;
        }
    }
    pub(crate) fn memory_bytes(&self) -> usize {
        self.matches.capacity() * std::mem::size_of::<Match>()
            + (self.filter.includes.capacity() + self.filter.excludes.capacity())
                * std::mem::size_of::<Vec<char>>()
            + self
                .filter
                .includes
                .iter()
                .chain(&self.filter.excludes)
                .map(|pattern| pattern.capacity() * std::mem::size_of::<char>())
                .sum::<usize>()
            + self.filter.sizes.capacity() * std::mem::size_of::<(Comparison, u64)>()
    }
}
fn combine(left: Match, right: Match) -> Match {
    match (left, right) {
        (Match::Yes, _) | (_, Match::Yes) => Match::Yes,
        (Match::Unknown, _) | (_, Match::Unknown) => Match::Unknown,
        _ => Match::No,
    }
}

fn parse_size(text: &str) -> Result<u64, FilterError> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(FilterError::InvalidSize);
    }
    let number = text[..digits]
        .parse::<u64>()
        .map_err(|_| FilterError::InvalidSize)?;
    let multiplier = match text[digits..].to_ascii_lowercase().as_str() {
        "b" => 1_u64,
        "kb" => 1024,
        "mb" => 1024 * 1024,
        "gb" => 1024 * 1024 * 1024,
        "tb" => 1024_u64.pow(4),
        _ => return Err(FilterError::InvalidSize),
    };
    number
        .checked_mul(multiplier)
        .ok_or(FilterError::InvalidSize)
}

fn wildcard(pattern: &[char], name: &[char]) -> bool {
    let (mut pattern_pos, mut name_pos) = (0, 0);
    let mut star = None;
    let mut retry_at = 0;
    while name_pos < name.len() {
        if pattern.get(pattern_pos) == Some(&'?') || pattern.get(pattern_pos) == name.get(name_pos)
        {
            pattern_pos += 1;
            name_pos += 1;
        } else if pattern.get(pattern_pos) == Some(&'*') {
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
    while pattern.get(pattern_pos) == Some(&'*') {
        pattern_pos += 1;
    }
    pattern_pos == pattern.len()
}
