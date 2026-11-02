use std::ffi::OsStr;

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

#[derive(Debug, Default)]
pub struct Filter {
    includes: Vec<Vec<char>>,
    excludes: Vec<Vec<char>>,
    sizes: Vec<(Comparison, u64)>,
}

#[derive(Debug)]
enum Comparison {
    Greater,
    Less,
}

impl Filter {
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
