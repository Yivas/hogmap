//! Explicit user-chosen locations, separate from application preferences.
//! JSON uses UTF-16 integer arrays so native Windows names round-trip without lossy conversion.
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;
pub const MAX_BOOKMARKS: usize = 64;
pub const MAX_NAME_CHARS: usize = 128;
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_COMPONENTS: usize = 512;
pub const MAX_MEMBERS: usize = 256;
pub const FILE_NAME: &str = "bookmarks.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub name: String,
    pub root: Option<PathBuf>,
    pub path: Vec<OsString>,
    pub members: Option<Vec<OsString>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    FutureVersion,
    Limit,
    Io(std::io::ErrorKind),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "Bookmark file is corrupt; ignored. Remove or clear bookmarks to replace it.",
            Self::FutureVersion => "Bookmark file version is unsupported; ignored and left unchanged until an explicit edit.",
            Self::Limit => "Bookmark limit exceeded (64 locations, 128 characters per name, 512 components, 256 members, 1 MiB file).",
            Self::Io(_) => "Cannot read or save bookmarks. Check access and free space; the previous file is retained.",
        })
    }
}
impl std::error::Error for Error {}

#[cfg(windows)]
fn wide(name: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    name.encode_wide().collect()
}
#[cfg(not(windows))]
fn wide(name: &OsStr) -> Vec<u16> {
    name.to_string_lossy().encode_utf16().collect()
}
#[cfg(windows)]
fn native(units: &[u16]) -> Result<OsString, Error> {
    use std::os::windows::ffi::OsStringExt;
    Ok(OsString::from_wide(units))
}
#[cfg(not(windows))]
fn native(units: &[u16]) -> Result<OsString, Error> {
    String::from_utf16(units)
        .map(Into::into)
        .map_err(|_| Error::Invalid)
}

fn validate(bookmark: &Bookmark) -> Result<(), Error> {
    if bookmark.name.trim().is_empty() || bookmark.name.chars().any(char::is_control) {
        return Err(Error::Invalid);
    }
    if bookmark.name.chars().count() > MAX_NAME_CHARS
        || bookmark.path.len() > MAX_COMPONENTS
        || bookmark
            .members
            .as_ref()
            .is_some_and(|m| m.is_empty() || m.len() > MAX_MEMBERS)
    {
        return Err(Error::Limit);
    }
    let mut length = 0;
    if let Some(root) = &bookmark.root {
        let units = wide(root.as_os_str());
        if !root.is_absolute() || units.contains(&0) || units.len() >= 32767 {
            return Err(Error::Invalid);
        }
        length = units.len();
    }
    for component in &bookmark.path {
        crate::index::validate_name(component).map_err(|_| Error::Invalid)?;
        length += wide(component).len() + 1;
    }
    if length >= 32767 {
        return Err(Error::Limit);
    }
    for member in bookmark.members.iter().flatten() {
        crate::index::validate_name(member).map_err(|_| Error::Invalid)?;
    }
    Ok(())
}
pub fn add(entries: &mut Vec<Bookmark>, bookmark: Bookmark) -> Result<(), Error> {
    validate(&bookmark)?;
    if entries.len() >= MAX_BOOKMARKS {
        return Err(Error::Limit);
    }
    entries.push(bookmark);
    if encode(entries).is_err() {
        entries.pop();
        return Err(Error::Limit);
    }
    Ok(())
}
fn units_json(name: &OsStr) -> String {
    format!(
        "[{}]",
        wide(name)
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn components_json(names: &[OsString]) -> String {
    format!(
        "[{}]",
        names
            .iter()
            .map(|name| units_json(name))
            .collect::<Vec<_>>()
            .join(",")
    )
}
pub fn encode(entries: &[Bookmark]) -> Result<String, Error> {
    if entries.len() > MAX_BOOKMARKS {
        return Err(Error::Limit);
    }
    // The empty document is the fixed part: `{"version":1,"bookmarks":[]}` plus the newline.
    // Each row adds its own text and, after the first, one separating comma, so the running
    // length is the length of the document this function would return. The cap is checked while
    // rows are built, so an oversized set is refused before the whole string exists.
    let mut length = "{\"version\":1,\"bookmarks\":[]}\n".len();
    let mut rows = Vec::new();
    for entry in entries {
        validate(entry)?;
        let row = format!(
            "[{},{},{},{}]",
            units_json(OsStr::new(&entry.name)),
            entry
                .root
                .as_ref()
                .map_or_else(|| "null".into(), |root| units_json(root.as_os_str())),
            components_json(&entry.path),
            entry
                .members
                .as_ref()
                .map_or_else(|| "null".into(), |m| components_json(m))
        );
        length += row.len() + usize::from(!rows.is_empty());
        if length > MAX_FILE_BYTES {
            return Err(Error::Limit);
        }
        rows.push(row);
    }
    Ok(format!(
        "{{\"version\":1,\"bookmarks\":[{}]}}\n",
        rows.join(",")
    ))
}

// Deliberately non-recursive schema reader: bounded arrays, no arbitrary object tree or strings.
struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn space(&mut self) {
        while self.0.first().is_some_and(u8::is_ascii_whitespace) {
            self.0 = &self.0[1..];
        }
    }
    fn eat(&mut self, token: &[u8]) -> bool {
        self.space();
        if self.0.starts_with(token) {
            self.0 = &self.0[token.len()..];
            true
        } else {
            false
        }
    }
    fn need(&mut self, token: &[u8]) -> Result<(), Error> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    fn number(&mut self) -> Result<u32, Error> {
        self.space();
        let length = self.0.iter().take_while(|b| b.is_ascii_digit()).count();
        if length == 0 || length > 10 || (length > 1 && self.0[0] == b'0') {
            return Err(Error::Invalid);
        }
        let value = std::str::from_utf8(&self.0[..length])
            .map_err(|_| Error::Invalid)?
            .parse()
            .map_err(|_| Error::Invalid)?;
        self.0 = &self.0[length..];
        Ok(value)
    }
    fn array<T>(
        &mut self,
        cap: usize,
        read: impl Fn(&mut Self) -> Result<T, Error>,
    ) -> Result<Vec<T>, Error> {
        self.need(b"[")?;
        let mut items = Vec::new();
        if self.eat(b"]") {
            return Ok(items);
        }
        loop {
            if items.len() == cap {
                return Err(Error::Limit);
            }
            items.push(read(self)?);
            if self.eat(b"]") {
                return Ok(items);
            }
            self.need(b",")?;
        }
    }
    fn name(&mut self, cap: usize) -> Result<OsString, Error> {
        let units = self.array(cap, |r| {
            u16::try_from(r.number()?).map_err(|_| Error::Invalid)
        })?;
        if units.contains(&0) {
            return Err(Error::Invalid);
        }
        native(&units)
    }
    fn bookmark(&mut self) -> Result<Bookmark, Error> {
        self.need(b"[")?;
        let name = self
            .name(MAX_NAME_CHARS * 2)?
            .into_string()
            .map_err(|_| Error::Invalid)?;
        self.need(b",")?;
        let root = if self.eat(b"null") {
            None
        } else {
            Some(PathBuf::from(self.name(32766)?))
        };
        self.need(b",")?;
        let path = self.array(MAX_COMPONENTS, |r| r.name(255))?;
        self.need(b",")?;
        let members = if self.eat(b"null") {
            None
        } else {
            Some(self.array(MAX_MEMBERS, |r| r.name(255))?)
        };
        self.need(b"]")?;
        let bookmark = Bookmark {
            name,
            root,
            path,
            members,
        };
        validate(&bookmark)?;
        Ok(bookmark)
    }
}
pub fn decode(bytes: &[u8]) -> Result<Vec<Bookmark>, Error> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::Limit);
    }
    let mut reader = Reader(bytes);
    reader.need(b"{")?;
    let mut version = None;
    let mut entries = None;
    loop {
        if reader.eat(b"\"version\"") && version.is_none() {
            reader.need(b":")?;
            let number = reader.number()?;
            if number != VERSION {
                return Err(Error::FutureVersion);
            }
            version = Some(number);
        } else if reader.eat(b"\"bookmarks\"") && entries.is_none() {
            reader.need(b":")?;
            entries = Some(reader.array(MAX_BOOKMARKS, Reader::bookmark)?);
        } else {
            return Err(Error::Invalid);
        }
        if reader.eat(b"}") {
            break;
        }
        reader.need(b",")?;
    }
    reader.space();
    if !reader.0.is_empty() || version.is_none() {
        return Err(Error::Invalid);
    }
    entries.ok_or(Error::Invalid)
}
pub fn load(path: &Path) -> Result<Vec<Bookmark>, Error> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::Io(error.kind())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Io(e.kind()))?;
    decode(&bytes)
}
pub fn save(path: &Path, entries: &[Bookmark]) -> Result<(), Error> {
    use std::io::Write;
    let json = encode(entries)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io(e.kind()))?;
    }
    crate::snapshot::write_file(path, |writer| {
        writer
            .write_all(json.as_bytes())
            .map_err(|e| crate::snapshot::Error::Write(e.kind()))
    })
    .map_err(|error| match error {
        crate::snapshot::Error::Write(kind) => Error::Io(kind),
        _ => Error::Invalid,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn bookmark() -> Bookmark {
        Bookmark {
            name: "Work 🐗".into(),
            root: Some(std::env::temp_dir().join("synthetic-root")),
            path: vec!["folder".into()],
            members: Some(vec!["one.bin".into()]),
        }
    }
    #[test]
    fn locations_round_trip_without_any_other_setting() {
        let entries = vec![bookmark()];
        let json = encode(&entries).unwrap();
        assert_eq!(decode(json.as_bytes()).unwrap(), entries);
        assert!(json.starts_with("{\"version\":1,\"bookmarks\":["));
        assert!(!json.contains("theme") && !json.contains("filter") && !json.contains("metric"));
    }
    #[test]
    fn corrupt_future_and_oversized_files_are_rejected_not_defaulted() {
        for bytes in [
            b"broken".as_slice(),
            b"{\"version\":1,\"bookmarks\":[[[1],null,[],null]],\"theme\":1}",
            b"{\"version\":1,\"bookmarks\":[]} garbage",
        ] {
            assert!(decode(bytes).is_err());
        }
        assert_eq!(
            decode(b"{\"version\":2,\"bookmarks\":[]}"),
            Err(Error::FutureVersion)
        );
        assert_eq!(decode(&vec![b' '; MAX_FILE_BYTES + 1]), Err(Error::Limit));
    }
    #[test]
    fn the_file_cap_counts_rows_as_they_are_built() {
        // 62 three-digit units per component keep the reconstructed path below its own 32767-unit
        // cap, so the file size is the limit under test. Eight such entries fit under 1 MiB and a
        // ninth does not, which is the boundary the running length has to measure: the fixed
        // header, every row and the comma between rows.
        let maximal = || Bookmark {
            name: "a".into(),
            root: None,
            path: vec!["x".repeat(62).into(); MAX_COMPONENTS],
            members: None,
        };
        let json = encode(&vec![maximal(); 8]).unwrap();
        assert!(json.len() <= MAX_FILE_BYTES);
        assert_eq!(decode(json.as_bytes()).unwrap().len(), 8);
        assert_eq!(encode(&vec![maximal(); 9]), Err(Error::Limit));
    }
    #[test]
    fn entry_name_and_member_caps_apply_before_saving() {
        let mut entries = Vec::new();
        for _ in 0..64 {
            add(&mut entries, bookmark()).unwrap();
        }
        assert_eq!(add(&mut entries, bookmark()), Err(Error::Limit));
        assert_eq!(entries.len(), 64);
        let mut entry = bookmark();
        entry.name = "a".repeat(129);
        assert_eq!(encode(&[entry]), Err(Error::Limit));
        let mut entry = bookmark();
        entry.members = Some(vec!["one".into(); 257]);
        assert_eq!(encode(&[entry]), Err(Error::Limit));
    }
}
