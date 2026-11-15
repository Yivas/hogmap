//! Versioned metadata files. Loaded paths are descriptive only; this module never visits them.
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::composition::VolumeEvidence;
use crate::filter::Metadata;
use crate::index::{EntryId, FileIdentity, Index, Kind, SizeMetric, Sizes, State};
use crate::scan::Terminal;
use crate::volume::*;

pub const MAGIC: &[u8; 8] = b"HOGMAP\0\0";
pub const VERSION: u32 = 1;
pub const MAX_ENTRIES: usize = 1_000_000;
pub const MAX_NAME_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_DEPTH: usize = 512;
pub const MAX_PATH_UNITS: usize = 32767;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    Entries,
    Names,
    File,
    Component,
    Path,
    Depth,
}
#[derive(Debug)]
pub enum Error {
    WrongFormat,
    Version(u32),
    Truncated,
    TooLarge(Limit),
    InvalidRecord,
    InvalidParent,
    TrailingData,
    Read(io::ErrorKind),
    Write(io::ErrorKind),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongFormat => f.write_str("This is not a HogMap snapshot. Choose a .hogmap file saved by HogMap."),
            Self::Version(version) => write!(f, "Snapshot version {version} is not supported (this build reads version 1). Use the HogMap build that saved it."),
            Self::Truncated => f.write_str("The snapshot is incomplete. Choose another copy or save the scan again."),
            Self::TooLarge(limit) => write!(f, "Snapshot limit exceeded: {}. Scan limits are separate; a larger scan cannot be saved in this format.", match limit {
                Limit::Entries => "1,000,000 entries", Limit::Names => "256 MiB of encoded names",
                Limit::File => "512 MiB per file", Limit::Component => "255 UTF-16 units per name",
                Limit::Path => "32767 UTF-16 units per root path", Limit::Depth => "512 nested levels",
            }),
            Self::InvalidRecord => f.write_str("The snapshot contains invalid metadata. Choose another copy or save the scan again."),
            Self::InvalidParent => f.write_str("The snapshot has an invalid folder hierarchy. Choose another copy."),
            Self::TrailingData => f.write_str("The snapshot has unexpected trailing data. Choose an unmodified HogMap snapshot."),
            Self::Read(kind) => write!(f, "Cannot read the snapshot ({kind}). Check access and choose a readable local file."),
            Self::Write(kind) => write!(f, "Cannot save the file ({kind}). Check permissions and free space, then choose another destination."),
        }
    }
}
impl std::error::Error for Error {}

/// The independent header observation, not a substitute for origin-bound capacity endpoints.
#[derive(Clone, Debug)]
pub struct Header {
    pub root: PathBuf,
    pub label: String,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub storage_reserve_bytes: Option<u64>,
    pub capacity: Option<CapacityMetadata>,
}
#[derive(Debug)]
pub struct Snapshot {
    /// None denotes the synthetic sample, not a filesystem path to be resolved on open.
    pub root_path: Option<PathBuf>,
    pub metric: SizeMetric,
    pub terminal: Terminal,
    pub index: Index,
    pub evidence: Option<VolumeEvidence>,
    pub header: Option<Header>,
}

struct Codec<T> {
    io: T,
    position: u64,
    length: u64,
}
impl<T> Codec<T> {
    fn new(io: T, length: u64) -> Self {
        Self {
            io,
            position: 0,
            length,
        }
    }
}
impl<T: Read> Codec<T> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.require(N as u64)?;
        let mut bytes = [0; N];
        self.io.read_exact(&mut bytes).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                Error::Truncated
            } else {
                Error::Read(e.kind())
            }
        })?;
        self.position += N as u64;
        Ok(bytes)
    }
    fn require(&self, bytes: u64) -> Result<(), Error> {
        if bytes > self.length.saturating_sub(self.position) {
            Err(Error::Truncated)
        } else {
            Ok(())
        }
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.bytes()?))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }
    fn u128(&mut self) -> Result<u128, Error> {
        Ok(u128::from_le_bytes(self.bytes()?))
    }
    fn boolean(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InvalidRecord),
        }
    }
    fn optional<V>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<V, Error>,
    ) -> Result<Option<V>, Error> {
        if self.boolean()? {
            read(self).map(Some)
        } else {
            Ok(None)
        }
    }
    fn result<V>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<V, Error>,
    ) -> Result<Result<V, OriginIssue>, Error> {
        if self.boolean()? {
            Ok(Ok(read(self)?))
        } else {
            Ok(Err(self.issue()?))
        }
    }
    fn name(&mut self, cap: usize, limit: Limit) -> Result<OsString, Error> {
        let count = self.u32()? as usize;
        if count > cap {
            return Err(Error::TooLarge(limit));
        }
        self.require(count as u64 * 2)?;
        let mut units = Vec::new();
        units
            .try_reserve_exact(count)
            .map_err(|_| Error::TooLarge(limit))?;
        for _ in 0..count {
            units.push(self.u16()?);
        }
        if units.contains(&0) {
            return Err(Error::InvalidRecord);
        }
        from_wide(&units)
    }
    fn time(&mut self) -> Result<SystemTime, Error> {
        let before = self.boolean()?;
        let seconds = self.u64()?;
        let nanos = self.u32()?;
        if nanos >= 1_000_000_000 {
            return Err(Error::InvalidRecord);
        }
        let duration = Duration::new(seconds, nanos);
        if before {
            UNIX_EPOCH.checked_sub(duration)
        } else {
            UNIX_EPOCH.checked_add(duration)
        }
        .ok_or(Error::InvalidRecord)
    }
    fn capacity_issue(&mut self) -> Result<CapacityIssue, Error> {
        Ok(match self.u8()? {
            0 => CapacityIssue::QueryFailed(self.u32()? as i32),
            1 => CapacityIssue::InvalidAllocationUnit,
            2 => CapacityIssue::SizeOverflow,
            3 => CapacityIssue::IncoherentFreeSpace,
            _ => return Err(Error::InvalidRecord),
        })
    }
    fn capacity(&mut self) -> Result<CapacityMetadata, Error> {
        if self.u8()? != 0 {
            return Err(Error::InvalidRecord);
        }
        Ok(CapacityMetadata {
            source: CapacitySource::WindowsActualGlobalFilesystem,
            captured_at: self.time()?,
            issue: self.optional(Self::capacity_issue)?,
        })
    }
    fn sample(&mut self) -> Result<OriginCapacitySample, Error> {
        Ok(OriginCapacitySample {
            total_bytes: self.optional(Self::u64)?,
            free_bytes: self.optional(Self::u64)?,
            storage_reserve_bytes: self.optional(Self::u64)?,
            total_reserved_bytes: self.optional(Self::u64)?,
            reserve_issue: self.optional(Self::capacity_issue)?,
            metadata: self.capacity()?,
        })
    }
    fn issue(&mut self) -> Result<OriginIssue, Error> {
        Ok(match self.u8()? {
            0 => OriginIssue::QueryFailed {
                query: match self.u8()? {
                    0 => OriginQuery::Attributes,
                    1 => OriginQuery::FinalPath,
                    2 => OriginQuery::FileIdentity,
                    3 => OriginQuery::Filesystem,
                    4 => OriginQuery::OpenVolumeRoot,
                    5 => OriginQuery::RetainRoot,
                    6 => OriginQuery::ErrorMode,
                    _ => return Err(Error::InvalidRecord),
                },
                code: self.u32()?,
            },
            1 => OriginIssue::UnsupportedNamespace,
            2 => OriginIssue::InvalidFinalPath,
            3 => OriginIssue::MissingFileIdentity,
            4 => OriginIssue::MissingFilesystem,
            5 => OriginIssue::UnsafeDirectory,
            6 => OriginIssue::UnprovenVolumeRelation,
            7 => OriginIssue::IdentityChanged,
            8 => OriginIssue::NamespaceChanged,
            9 => OriginIssue::Cancelled,
            10 => OriginIssue::ScanNotFinished,
            11 => OriginIssue::RootNotCaptured,
            12 => OriginIssue::UnsupportedPlatform,
            _ => return Err(Error::InvalidRecord),
        })
    }
    fn continuity(&mut self) -> Result<ObservedContinuity, Error> {
        match self.u8()? {
            0 => Ok(ObservedContinuity::UnchangedAtEndpoints),
            1 => Ok(ObservedContinuity::Changed),
            _ => Err(Error::InvalidRecord),
        }
    }
    fn evidence(&mut self) -> Result<VolumeEvidence, Error> {
        Ok(VolumeEvidence {
            scope: self.result(|r| match r.u8()? {
                0 => Ok(RootScope::WholeVolume),
                1 => Ok(RootScope::Folder),
                2 => Ok(RootScope::Unknown),
                _ => Err(Error::InvalidRecord),
            })?,
            filesystem: self.result(|r| match r.u8()? {
                0 => Ok(ObservedFilesystem::Ntfs),
                1 => Ok(ObservedFilesystem::Other(
                    r.name(261, Limit::Component)?
                        .into_string()
                        .map_err(|_| Error::InvalidRecord)?,
                )),
                _ => Err(Error::InvalidRecord),
            })?,
            before: self.result(Self::sample)?,
            after: self.result(Self::sample)?,
            validation: self.result(|r| {
                Ok(OriginValidation {
                    directory_identity: r.result(Self::continuity)?,
                    normalized_path: r.result(Self::continuity)?,
                })
            })?,
            directories: DirectoryNamespaceObservations {
                opened_directories: self.u64()?,
                serial64_same: self.u64()?,
                serial64_different: self.u64()?,
                serial64_unavailable: self.u64()?,
                guid_on_serial_mismatch: self.u64()?,
                guid_on_serial_unavailable: self.u64()?,
                same_guid_name: self.u64()?,
                different_guid_name_unproven: self.u64()?,
                unavailable: self.u64()?,
                not_queried: self.u64()?,
                last_issue: self.optional(Self::issue)?,
            },
        })
    }
    fn sizes(&mut self) -> Result<Sizes, Error> {
        Ok(Sizes {
            logical: self.optional(Self::u64)?,
            allocated: self.optional(Self::u64)?,
        })
    }
    fn identity(&mut self) -> Result<FileIdentity, Error> {
        Ok(FileIdentity {
            volume: self.u64()?,
            file: self.u128()?,
        })
    }
}

#[cfg(windows)]
fn from_wide(units: &[u16]) -> Result<OsString, Error> {
    use std::os::windows::ffi::OsStringExt;
    Ok(OsString::from_wide(units))
}
#[cfg(not(windows))]
fn from_wide(units: &[u16]) -> Result<OsString, Error> {
    String::from_utf16(units)
        .map(Into::into)
        .map_err(|_| Error::InvalidRecord)
}
#[cfg(windows)]
fn wide(name: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    name.encode_wide().collect()
}
#[cfg(not(windows))]
fn wide(name: &OsStr) -> Vec<u16> {
    name.to_string_lossy().encode_utf16().collect()
}

struct Encoder<T> {
    io: T,
    position: u64,
}
impl<T: Write> Encoder<T> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.position + bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::TooLarge(Limit::File));
        }
        self.io
            .write_all(bytes)
            .map_err(|e| Error::Write(e.kind()))?;
        self.position += bytes.len() as u64;
        Ok(())
    }
    fn u8(&mut self, value: u8) -> Result<(), Error> {
        self.bytes(&[value])
    }
    fn u32(&mut self, value: u32) -> Result<(), Error> {
        self.bytes(&value.to_le_bytes())
    }
    fn u64(&mut self, value: u64) -> Result<(), Error> {
        self.bytes(&value.to_le_bytes())
    }
    fn boolean(&mut self, value: bool) -> Result<(), Error> {
        self.u8(u8::from(value))
    }
    fn optional<V>(
        &mut self,
        value: Option<V>,
        write: impl FnOnce(&mut Self, V) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.boolean(value.is_some())?;
        if let Some(value) = value {
            write(self, value)?;
        }
        Ok(())
    }
    fn result<V>(
        &mut self,
        value: Result<V, OriginIssue>,
        write: impl FnOnce(&mut Self, V) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.boolean(value.is_ok())?;
        match value {
            Ok(value) => write(self, value),
            Err(issue) => self.issue(issue),
        }
    }
    fn name(&mut self, name: &OsStr) -> Result<(), Error> {
        let units = wide(name);
        self.u32(units.len() as u32)?;
        for unit in units {
            self.bytes(&unit.to_le_bytes())?;
        }
        Ok(())
    }
    fn time(&mut self, time: SystemTime) -> Result<(), Error> {
        let duration = time.duration_since(UNIX_EPOCH);
        self.boolean(duration.is_err())?;
        let duration = duration.unwrap_or_else(|e| e.duration());
        self.u64(duration.as_secs())?;
        self.u32(duration.subsec_nanos())
    }
    fn capacity_issue(&mut self, issue: CapacityIssue) -> Result<(), Error> {
        match issue {
            CapacityIssue::QueryFailed(code) => {
                self.u8(0)?;
                self.u32(code as u32)
            }
            CapacityIssue::InvalidAllocationUnit => self.u8(1),
            CapacityIssue::SizeOverflow => self.u8(2),
            CapacityIssue::IncoherentFreeSpace => self.u8(3),
        }
    }
    fn capacity(&mut self, metadata: CapacityMetadata) -> Result<(), Error> {
        self.u8(0)?;
        self.time(metadata.captured_at)?;
        self.optional(metadata.issue, Self::capacity_issue)
    }
    fn sample(&mut self, sample: OriginCapacitySample) -> Result<(), Error> {
        for bytes in [
            sample.total_bytes,
            sample.free_bytes,
            sample.storage_reserve_bytes,
            sample.total_reserved_bytes,
        ] {
            self.optional(bytes, Self::u64)?;
        }
        self.optional(sample.reserve_issue, Self::capacity_issue)?;
        self.capacity(sample.metadata)
    }
    fn issue(&mut self, issue: OriginIssue) -> Result<(), Error> {
        self.u8(match issue {
            OriginIssue::QueryFailed { .. } => 0,
            OriginIssue::UnsupportedNamespace => 1,
            OriginIssue::InvalidFinalPath => 2,
            OriginIssue::MissingFileIdentity => 3,
            OriginIssue::MissingFilesystem => 4,
            OriginIssue::UnsafeDirectory => 5,
            OriginIssue::UnprovenVolumeRelation => 6,
            OriginIssue::IdentityChanged => 7,
            OriginIssue::NamespaceChanged => 8,
            OriginIssue::Cancelled => 9,
            OriginIssue::ScanNotFinished => 10,
            OriginIssue::RootNotCaptured => 11,
            OriginIssue::UnsupportedPlatform => 12,
        })?;
        if let OriginIssue::QueryFailed { query, code } = issue {
            self.u8(match query {
                OriginQuery::Attributes => 0,
                OriginQuery::FinalPath => 1,
                OriginQuery::FileIdentity => 2,
                OriginQuery::Filesystem => 3,
                OriginQuery::OpenVolumeRoot => 4,
                OriginQuery::RetainRoot => 5,
                OriginQuery::ErrorMode => 6,
            })?;
            self.u32(code)?;
        }
        Ok(())
    }
    fn continuity(&mut self, value: ObservedContinuity) -> Result<(), Error> {
        self.u8(match value {
            ObservedContinuity::UnchangedAtEndpoints => 0,
            ObservedContinuity::Changed => 1,
        })
    }
    fn evidence(&mut self, value: &VolumeEvidence) -> Result<(), Error> {
        self.result(value.scope, |w, scope| {
            w.u8(match scope {
                RootScope::WholeVolume => 0,
                RootScope::Folder => 1,
                RootScope::Unknown => 2,
            })
        })?;
        self.result(
            value.filesystem.as_ref().map_err(|e| *e),
            |w, filesystem| match filesystem {
                ObservedFilesystem::Ntfs => w.u8(0),
                ObservedFilesystem::Other(name) => {
                    w.u8(1)?;
                    w.name(OsStr::new(name))
                }
            },
        )?;
        self.result(value.before, Self::sample)?;
        self.result(value.after, Self::sample)?;
        self.result(value.validation, |w, validation| {
            w.result(validation.directory_identity, Self::continuity)?;
            w.result(validation.normalized_path, Self::continuity)
        })?;
        let d = value.directories;
        for count in [
            d.opened_directories,
            d.serial64_same,
            d.serial64_different,
            d.serial64_unavailable,
            d.guid_on_serial_mismatch,
            d.guid_on_serial_unavailable,
            d.same_guid_name,
            d.different_guid_name_unproven,
            d.unavailable,
            d.not_queried,
        ] {
            self.u64(count)?;
        }
        self.optional(d.last_issue, Self::issue)
    }
    fn sizes(&mut self, value: Sizes) -> Result<(), Error> {
        self.optional(value.logical, Self::u64)?;
        self.optional(value.allocated, Self::u64)
    }
    fn identity(&mut self, value: FileIdentity) -> Result<(), Error> {
        self.u64(value.volume)?;
        self.bytes(&value.file.to_le_bytes())
    }
}

/// Write beside the chosen destination, flush, then rename; failure leaves an existing file intact.
/// Only this operation's create_new temporary file is removed on failure.
pub fn write_file(
    path: &std::path::Path,
    write: impl FnOnce(&mut io::BufWriter<std::fs::File>) -> Result<(), Error>,
) -> Result<(), Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temporary = path.with_file_name(format!(
        ".hogmap-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| Error::Write(e.kind()))?;
    let result = (|| {
        let mut writer = io::BufWriter::new(file);
        write(&mut writer)?;
        writer.flush().map_err(|e| Error::Write(e.kind()))?;
        writer
            .get_ref()
            .sync_all()
            .map_err(|e| Error::Write(e.kind()))?;
        drop(writer);
        std::fs::rename(&temporary, path).map_err(|e| Error::Write(e.kind()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Borrowed save input avoids cloning an index. Save is explicit, never application persistence.
pub struct Save<'a> {
    pub root_path: Option<&'a std::path::Path>,
    pub metric: SizeMetric,
    pub terminal: Terminal,
    pub index: &'a Index,
    pub evidence: Option<&'a VolumeEvidence>,
    pub header: Option<&'a Header>,
}

pub fn encode(output: impl Write, save: &Save<'_>) -> Result<(), Error> {
    validate_save(save)?;
    let mut w = Encoder {
        io: output,
        position: 0,
    };
    w.bytes(MAGIC)?;
    w.u32(VERSION)?;
    w.optional(save.root_path, |w, path| w.name(path.as_os_str()))?;
    w.u8(match save.metric {
        SizeMetric::Logical => 0,
        SizeMetric::Allocated => 1,
    })?;
    match save.terminal {
        Terminal::Finished => w.u8(0)?,
        Terminal::Cancelled => w.u8(1)?,
        Terminal::Failed(code) => {
            w.u8(2)?;
            w.u32(code)?;
        }
    }
    w.optional(save.evidence, |w, e| w.evidence(e))?;
    w.optional(save.header, |w, h| {
        w.name(h.root.as_os_str())?;
        w.name(OsStr::new(&h.label))?;
        w.optional(h.free_bytes, Encoder::u64)?;
        w.optional(h.total_bytes, Encoder::u64)?;
        w.optional(h.storage_reserve_bytes, Encoder::u64)?;
        w.optional(h.capacity, Encoder::capacity)
    })?;
    w.u32(save.index.len() as u32)?;
    for (id, entry) in save.index.entries() {
        w.u32(entry.parent.map_or(u32::MAX, |id| id.position() as u32))?;
        w.name(entry.name)?;
        w.u8(match entry.kind {
            Kind::Directory => 0,
            Kind::File => 1,
        })?;
        w.sizes(entry.sizes)?;
        w.u8(match entry.state {
            State::Pending => 0,
            State::Complete => 1,
            State::Partial => 2,
            State::Inaccessible => 3,
            State::Stale => 4,
        })?;
        w.optional(entry.identity, Encoder::identity)?;
        let metadata = save.index.filter_metadata(id);
        for ticks in [metadata.created, metadata.modified, metadata.accessed] {
            w.u64(ticks)?;
        }
        w.optional(metadata.attributes, Encoder::u32)?;
        w.optional(metadata.ads, Encoder::boolean)?;
        w.optional(save.index.tag_assignment(id), Encoder::u8)?;
    }
    let corrections = save.index.authoritative_corrections();
    w.u32(corrections.len() as u32)?;
    for (identity, sizes) in corrections {
        w.identity(identity)?;
        w.sizes(sizes)?;
    }
    Ok(())
}
fn validate_save(save: &Save<'_>) -> Result<(), Error> {
    if save.index.len() > MAX_ENTRIES {
        return Err(Error::TooLarge(Limit::Entries));
    }
    for path in [save.root_path, save.header.map(|h| h.root.as_path())]
        .into_iter()
        .flatten()
    {
        if wide(path.as_os_str()).len() > MAX_PATH_UNITS {
            return Err(Error::TooLarge(Limit::Path));
        }
    }
    if save
        .header
        .is_some_and(|h| h.label.encode_utf16().count() > 261)
    {
        return Err(Error::TooLarge(Limit::Component));
    }
    let mut names = 0usize;
    let mut depths = Vec::new();
    for (_, entry) in save.index.entries() {
        names = names
            .checked_add(entry.name.as_encoded_bytes().len())
            .ok_or(Error::TooLarge(Limit::Names))?;
        if names > MAX_NAME_BYTES {
            return Err(Error::TooLarge(Limit::Names));
        }
        let depth = entry
            .parent
            .map_or(0, |parent| depths[parent.position()] + 1);
        if depth > MAX_DEPTH {
            return Err(Error::TooLarge(Limit::Depth));
        }
        depths.push(depth);
    }
    Ok(())
}

/// Two passes: validate the entire hierarchy and tail before building an index or its hash tables.
/// No supplied path is resolved, and no stored observation is authenticated by this format.
pub fn decode(mut input: impl Read + Seek) -> Result<Snapshot, Error> {
    let length = input
        .seek(SeekFrom::End(0))
        .map_err(|e| Error::Read(e.kind()))?;
    if length > MAX_FILE_BYTES {
        return Err(Error::TooLarge(Limit::File));
    }
    input
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::Read(e.kind()))?;
    parse(&mut input, length, false)?;
    input
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::Read(e.kind()))?;
    parse(&mut input, length, true)?.ok_or(Error::InvalidRecord)
}
fn parse(input: impl Read, length: u64, build: bool) -> Result<Option<Snapshot>, Error> {
    let mut r = Codec::new(input, length);
    if &r.bytes::<8>()? != MAGIC {
        return Err(Error::WrongFormat);
    }
    let version = r.u32()?;
    if version != VERSION {
        return Err(Error::Version(version));
    }
    let root_path = r.optional(|r| r.name(MAX_PATH_UNITS, Limit::Path).map(PathBuf::from))?;
    let metric = match r.u8()? {
        0 => SizeMetric::Logical,
        1 => SizeMetric::Allocated,
        _ => return Err(Error::InvalidRecord),
    };
    let terminal = match r.u8()? {
        0 => Terminal::Finished,
        1 => Terminal::Cancelled,
        2 => Terminal::Failed(r.u32()?),
        _ => return Err(Error::InvalidRecord),
    };
    let evidence = r.optional(Codec::evidence)?;
    let header = r.optional(|r| {
        Ok(Header {
            root: PathBuf::from(r.name(MAX_PATH_UNITS, Limit::Path)?),
            label: r
                .name(261, Limit::Component)?
                .into_string()
                .map_err(|_| Error::InvalidRecord)?,
            free_bytes: r.optional(Codec::u64)?,
            total_bytes: r.optional(Codec::u64)?,
            storage_reserve_bytes: r.optional(Codec::u64)?,
            capacity: r.optional(Codec::capacity)?,
        })
    })?;
    let count = r.u32()? as usize;
    if count == 0 {
        return Err(Error::InvalidRecord);
    }
    if count > MAX_ENTRIES {
        return Err(Error::TooLarge(Limit::Entries));
    }
    // Minimum record: parent4 + length4 + one UTF16 unit2 + kind1 + sizes2 + state1
    // + identity1 + timestamps24 + attrs1 + ads1 + tag1 = 42 bytes.
    r.require(count as u64 * 42 + 4)?;
    let mut hierarchy: Vec<(Kind, usize)> = Vec::new();
    hierarchy
        .try_reserve_exact(count)
        .map_err(|_| Error::TooLarge(Limit::Entries))?;
    let mut names = 0usize;
    let mut index: Option<Index> = None;
    for position in 0..count {
        let parent = r.u32()? as usize;
        let depth = if position == 0 {
            if parent != u32::MAX as usize {
                return Err(Error::InvalidParent);
            }
            0
        } else {
            let (kind, depth) = hierarchy.get(parent).ok_or(Error::InvalidParent)?;
            if *kind != Kind::Directory {
                return Err(Error::InvalidParent);
            }
            depth + 1
        };
        if depth > MAX_DEPTH {
            return Err(Error::TooLarge(Limit::Depth));
        }
        let name = r.name(255, Limit::Component)?;
        names = names
            .checked_add(name.as_encoded_bytes().len())
            .ok_or(Error::TooLarge(Limit::Names))?;
        if names > MAX_NAME_BYTES {
            return Err(Error::TooLarge(Limit::Names));
        }
        crate::index::validate_name(&name).map_err(|_| Error::InvalidRecord)?;
        let kind = match r.u8()? {
            0 => Kind::Directory,
            1 => Kind::File,
            _ => return Err(Error::InvalidRecord),
        };
        let sizes = r.sizes()?;
        let state = match r.u8()? {
            0 => State::Pending,
            1 => State::Complete,
            2 => State::Partial,
            3 => State::Inaccessible,
            4 => State::Stale,
            _ => return Err(Error::InvalidRecord),
        };
        let identity = r.optional(Codec::identity)?;
        if position == 0
            && (kind != Kind::Directory || sizes != Sizes::default() || identity.is_some())
        {
            return Err(Error::InvalidRecord);
        }
        let metadata = Metadata {
            created: r.u64()?,
            modified: r.u64()?,
            accessed: r.u64()?,
            attributes: r.optional(Codec::u32)?,
            ads: r.optional(Codec::boolean)?,
        };
        let tag = r.optional(Codec::u8)?;
        if tag.is_some_and(|bits| bits > 15) {
            return Err(Error::InvalidRecord);
        }
        hierarchy.push((kind, depth));
        if build {
            let id = if position == 0 {
                index = Some(Index::new(name).map_err(|_| Error::InvalidRecord)?);
                index.as_ref().unwrap().root()
            } else {
                index
                    .as_mut()
                    .unwrap()
                    .insert(
                        EntryId::at(parent).ok_or(Error::InvalidParent)?,
                        name,
                        kind,
                        sizes,
                        identity,
                        state,
                    )
                    .map_err(|_| Error::InvalidRecord)?
            };
            let index = index.as_mut().unwrap();
            if position == 0 {
                index
                    .set_state(id, state)
                    .map_err(|_| Error::InvalidRecord)?;
            }
            index
                .set_filter_metadata(id, metadata)
                .map_err(|_| Error::InvalidRecord)?;
            if let Some(tag) = tag {
                index.set_tags(id, tag).map_err(|_| Error::InvalidRecord)?;
            }
        }
    }
    let corrections = r.u32()? as usize;
    if corrections > count {
        return Err(Error::TooLarge(Limit::Entries));
    }
    r.require(corrections as u64 * 26)?;
    let mut previous = None;
    for _ in 0..corrections {
        let identity = r.identity()?;
        let key = (identity.volume, identity.file);
        if previous.is_some_and(|previous| previous >= key) {
            return Err(Error::InvalidRecord);
        }
        previous = Some(key);
        let sizes = r.sizes()?;
        if let Some(index) = &mut index {
            index
                .apply_authoritative_sizes(identity, sizes)
                .map_err(|_| Error::InvalidRecord)?;
        }
    }
    if r.position != length {
        return Err(Error::TrailingData);
    }
    Ok(index.map(|index| Snapshot {
        root_path,
        metric,
        terminal,
        index,
        evidence,
        header,
    }))
}
