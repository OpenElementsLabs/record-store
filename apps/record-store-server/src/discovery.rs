//! Read-only discovery of storage this node could use.
//!
//! Discovery never enrolls, formats, mounts, or claims anything. It answers one
//! question — "what could I put a device on?" — so an administrator can write a
//! `[[storage.devices]]` entry without guessing at paths. Nothing here changes
//! cluster state, and a discovered path participates only once somebody declares
//! it.
//!
//! The filtering is a pure function over mount entries so it can be tested
//! without a machine that happens to have the right disks in it. Only the entry
//! source is platform specific.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use record_store_cluster::{
    DeviceCapacity, DeviceDiscovery, DeviceDiscoveryError, DeviceHealth, DeviceKind,
    DiscoveredDevice, HardwareMetadata,
};

/// One mounted filesystem, as the platform describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// Backing device, for example `/dev/sda1`. Descriptive only.
    pub source: String,
    /// Where the filesystem is mounted.
    pub mount_point: PathBuf,
    /// Filesystem type, for example `ext4`.
    pub filesystem: String,
    /// Mount options exactly as reported.
    pub options: Vec<String>,
}

/// Filesystems that never hold object payloads.
///
/// These are kernel and runtime filesystems: memory-backed, virtual, or owned by
/// the container runtime. Offering one as storage would be offering RAM, or a
/// path that disappears on restart.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fuse.gvfsd-fuse",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "overlay",
    "proc",
    "pstore",
    "ramfs",
    "securityfs",
    "selinuxfs",
    "squashfs",
    "sysfs",
    "tmpfs",
    "tracefs",
];

/// Mount points Record Store must never propose.
///
/// The root and boot filesystems are the operating system. An administrator can
/// still declare any path explicitly — this list governs what is *suggested*,
/// and suggesting the root filesystem as object storage is how a full disk takes
/// the machine down with it.
const PROTECTED_MOUNTS: &[&str] = &["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/etc"];

/// Returns whether a mount could plausibly hold Record Store payloads.
fn is_candidate(entry: &MountEntry) -> bool {
    if PSEUDO_FILESYSTEMS.contains(&entry.filesystem.as_str()) {
        return false;
    }
    if PROTECTED_MOUNTS.contains(&entry.mount_point.to_string_lossy().as_ref()) {
        return false;
    }
    // A read-only mount cannot take writes, and proposing one would produce a
    // device that fails on its first placement.
    if entry.options.iter().any(|option| option == "ro") {
        return false;
    }
    // Anything under a protected mount that is not itself a separate filesystem
    // has already been excluded by the mount table; what remains under /usr or
    // /etc is a bind mount of system state.
    PROTECTED_MOUNTS
        .iter()
        .filter(|protected| **protected != "/")
        .all(|protected| !entry.mount_point.starts_with(protected))
}

/// Maps a rotational hint onto a device kind.
fn kind_for(rotational: Option<bool>) -> DeviceKind {
    match rotational {
        // A filesystem is what Record Store actually writes to. The rotational
        // hint refines it only when the platform supplied one; guessing NVMe
        // from a device name would be inventing hardware facts.
        Some(true) => DeviceKind::Hdd,
        Some(false) => DeviceKind::Ssd,
        None => DeviceKind::FilesystemDirectory,
    }
}

/// Filters mount entries down to devices worth proposing.
///
/// `excluded` are paths already in use — the node's data directory and its
/// declared devices — so discovery never suggests something already registered.
#[must_use]
pub fn candidates(
    mounts: &[MountEntry],
    excluded: &[PathBuf],
    measure: &dyn Fn(&Path) -> (u64, u64),
    rotational: &dyn Fn(&str) -> Option<bool>,
) -> Vec<DiscoveredDevice> {
    let excluded: BTreeSet<&Path> = excluded.iter().map(PathBuf::as_path).collect();
    let mut seen = BTreeSet::new();
    let mut discovered = Vec::new();
    for entry in mounts {
        if !is_candidate(entry) {
            continue;
        }
        if excluded.contains(entry.mount_point.as_path()) {
            continue;
        }
        if !seen.insert(entry.mount_point.clone()) {
            continue;
        }
        let (total, available) = measure(&entry.mount_point);
        discovered.push(DiscoveredDevice {
            current_path: entry.mount_point.clone(),
            // The backing device name is recorded as a hint, never as identity:
            // it can change across reboots.
            stable_hardware_identifier: None,
            kind: kind_for(rotational(&entry.source)),
            capacity: DeviceCapacity {
                raw_bytes: total,
                usable_bytes: total,
                allocated_bytes: 0,
                reserved_bytes: 0,
                available_bytes: available,
            },
            // Nothing has inspected this hardware's health, and reporting
            // `Healthy` would be a reading nobody took.
            health: DeviceHealth::Unknown,
            hardware: HardwareMetadata {
                filesystem: Some(entry.filesystem.clone()),
                mount_point: Some(entry.mount_point.clone()),
                rotational: rotational(&entry.source),
                ..HardwareMetadata::default()
            },
        });
    }
    discovered.sort_by(|left, right| left.current_path.cmp(&right.current_path));
    discovered
}

/// Discovers mounted filesystems this node could store payloads on.
pub struct MountDiscovery {
    excluded: Vec<PathBuf>,
}

impl MountDiscovery {
    /// Creates a discovery that never proposes the supplied paths.
    #[must_use]
    pub fn new(excluded: Vec<PathBuf>) -> Self {
        Self { excluded }
    }
}

#[async_trait]
impl DeviceDiscovery for MountDiscovery {
    async fn discover(&self) -> Result<Vec<DiscoveredDevice>, DeviceDiscoveryError> {
        let mounts = read_mounts()?;
        Ok(candidates(
            &mounts,
            &self.excluded,
            &|path| {
                (
                    fs2::total_space(path).unwrap_or_default(),
                    fs2::available_space(path).unwrap_or_default(),
                )
            },
            &rotational_hint,
        ))
    }
}

/// Reads the platform's mount table.
#[cfg(target_os = "linux")]
fn read_mounts() -> Result<Vec<MountEntry>, DeviceDiscoveryError> {
    let table = std::fs::read_to_string("/proc/mounts")
        .map_err(|error| DeviceDiscoveryError(format!("could not read /proc/mounts: {error}")))?;
    Ok(parse_proc_mounts(&table))
}

/// Other platforms expose no equivalent Record Store can read without extra
/// privileges, so discovery reports that rather than guessing.
#[cfg(not(target_os = "linux"))]
fn read_mounts() -> Result<Vec<MountEntry>, DeviceDiscoveryError> {
    Err(DeviceDiscoveryError(
        "mount discovery is implemented for Linux only; declare devices explicitly".to_owned(),
    ))
}

/// Parses `/proc/mounts` into entries.
#[must_use]
pub fn parse_proc_mounts(table: &str) -> Vec<MountEntry> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let mount_point = fields.next()?;
            let filesystem = fields.next()?;
            let options = fields.next().unwrap_or_default();
            Some(MountEntry {
                source: source.to_owned(),
                // Octal escapes are how the kernel encodes spaces in a path.
                mount_point: PathBuf::from(unescape(mount_point)),
                filesystem: filesystem.to_owned(),
                options: options.split(',').map(str::to_owned).collect(),
            })
        })
        .collect()
}

/// Decodes the octal escapes the kernel writes into mount paths.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut bytes = value.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte as char);
            continue;
        }
        let digits: String = (0..3)
            .filter_map(|_| bytes.next().map(|b| b as char))
            .collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(decoded) => out.push(decoded as char),
            Err(_) => {
                out.push('\\');
                out.push_str(&digits);
            }
        }
    }
    out
}

/// Reports whether a backing device is rotational, when Linux says so.
#[cfg(target_os = "linux")]
fn rotational_hint(source: &str) -> Option<bool> {
    // `/dev/sda1` is backed by `sda`, whose queue exposes the flag. Partition
    // digits are stripped; anything that is not a plain block path is unknown
    // rather than assumed.
    let name = source.strip_prefix("/dev/")?;
    let base = name.trim_end_matches(|character: char| character.is_ascii_digit());
    let base = base.strip_suffix('p').unwrap_or(base);
    let flag = std::fs::read_to_string(format!("/sys/block/{base}/queue/rotational")).ok()?;
    match flag.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn rotational_hint(_source: &str) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(source: &str, point: &str, filesystem: &str, options: &str) -> MountEntry {
        MountEntry {
            source: source.to_owned(),
            mount_point: PathBuf::from(point),
            filesystem: filesystem.to_owned(),
            options: options.split(',').map(str::to_owned).collect(),
        }
    }

    fn measured(_path: &Path) -> (u64, u64) {
        (1_000, 400)
    }

    fn unknown_rotation(_source: &str) -> Option<bool> {
        None
    }

    /// Discovery must never propose the operating system's own filesystems.
    ///
    /// Offering the root filesystem as object storage is how a full disk takes
    /// the machine down with it, and offering tmpfs is offering RAM.
    #[test]
    fn system_and_memory_filesystems_are_never_proposed() {
        let mounts = vec![
            mount("/dev/sda1", "/", "ext4", "rw,relatime"),
            mount("/dev/sda2", "/boot", "ext4", "rw"),
            mount("/dev/sda3", "/boot/efi", "vfat", "rw"),
            mount("tmpfs", "/run", "tmpfs", "rw"),
            mount("proc", "/proc", "proc", "rw"),
            mount("sysfs", "/sys", "sysfs", "rw"),
            mount(
                "overlay",
                "/var/lib/docker/overlay2/x/merged",
                "overlay",
                "rw",
            ),
            mount("/dev/sdb1", "/mnt/data", "ext4", "rw,relatime"),
        ];

        let found = candidates(&mounts, &[], &measured, &unknown_rotation);
        let paths: Vec<_> = found
            .iter()
            .map(|device| device.current_path.clone())
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/mnt/data")]);
    }

    /// A read-only mount would produce a device that fails on its first write.
    #[test]
    fn read_only_mounts_are_not_proposed() {
        let mounts = vec![
            mount("/dev/sdb1", "/mnt/archive", "ext4", "ro,relatime"),
            mount("/dev/sdc1", "/mnt/data", "ext4", "rw"),
        ];

        let found = candidates(&mounts, &[], &measured, &unknown_rotation);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].current_path, PathBuf::from("/mnt/data"));
    }

    /// Anything already in use is not a candidate, or an operator would be
    /// invited to declare the same storage twice.
    #[test]
    fn paths_already_in_use_are_excluded() {
        let mounts = vec![
            mount("/dev/sdb1", "/var/lib/record-store", "ext4", "rw"),
            mount("/dev/sdc1", "/mnt/nvme0", "ext4", "rw"),
            mount("/dev/sdd1", "/mnt/spare", "ext4", "rw"),
        ];
        let excluded = vec![
            PathBuf::from("/var/lib/record-store"),
            PathBuf::from("/mnt/nvme0"),
        ];

        let found = candidates(&mounts, &excluded, &measured, &unknown_rotation);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].current_path, PathBuf::from("/mnt/spare"));
    }

    /// The kind reflects what the platform actually reported. Guessing NVMe from
    /// a device name would be inventing a hardware fact.
    #[test]
    fn device_kind_follows_the_platform_and_is_unknown_otherwise() {
        let mounts = vec![mount("/dev/sdb1", "/mnt/data", "ext4", "rw")];

        let spinning = candidates(&mounts, &[], &measured, &|_| Some(true));
        assert_eq!(spinning[0].kind, DeviceKind::Hdd);
        assert_eq!(spinning[0].hardware.rotational, Some(true));

        let solid = candidates(&mounts, &[], &measured, &|_| Some(false));
        assert_eq!(solid[0].kind, DeviceKind::Ssd);

        let unknown = candidates(&mounts, &[], &measured, &unknown_rotation);
        assert_eq!(unknown[0].kind, DeviceKind::FilesystemDirectory);
        assert_eq!(unknown[0].hardware.rotational, None);
    }

    /// Discovery reports capacity but never health: nothing has inspected the
    /// hardware, and `Healthy` would be a reading nobody took.
    #[test]
    fn discovery_reports_capacity_but_never_invents_health() {
        let mounts = vec![mount("/dev/sdb1", "/mnt/data", "ext4", "rw")];
        let found = candidates(&mounts, &[], &measured, &unknown_rotation);

        assert_eq!(found[0].capacity.raw_bytes, 1_000);
        assert_eq!(found[0].capacity.available_bytes, 400);
        assert_eq!(found[0].health, DeviceHealth::Unknown);
        assert_eq!(found[0].hardware.temperature_celsius, None);
        assert_eq!(found[0].stable_hardware_identifier, None);
    }

    #[test]
    fn proc_mounts_are_parsed_including_escaped_paths() {
        let table = concat!(
            "/dev/sda1 / ext4 rw,relatime 0 0\n",
            "/dev/sdb1 /mnt/my\\040disk ext4 rw 0 0\n",
        );
        let parsed = parse_proc_mounts(table);

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].filesystem, "ext4");
        assert!(parsed[0].options.contains(&"relatime".to_owned()));
        // A space in a mount path is escaped by the kernel; reading it literally
        // would silently address the wrong directory.
        assert_eq!(parsed[1].mount_point, PathBuf::from("/mnt/my disk"));
    }
}
