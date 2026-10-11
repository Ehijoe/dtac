//! Offline IP → ASN lookup used to flag datacenter / hosting-provider traffic.
//!
//! Range data is read from the public-domain [iptoasn.com](https://iptoasn.com/data/)
//! `ip2asn-v4.tsv`, `ip2asn-v6.tsv` or `ip2asn-combined.tsv` files, whose rows are
//! tab-separated `range_start  range_end  AS_number  country_code  AS_description`.
//! Lookups are a binary search over ranges sorted by start address, so they cost
//! `O(log n)` with no allocation.

use crate::error::{DTACError, Result};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::net::IpAddr;
use std::path::Path;
use tracing::warn;

/// Curated hosting / cloud-provider ASNs treated as "datacenter" by default.
///
/// This list is deliberately short and is **not** exhaustive; pass a custom list via
/// [`AsnDatabase::with_datacenter_asns`] for real deployments.
pub const DEFAULT_DATACENTER_ASNS: &[u32] = &[
    16509,   // Amazon AWS
    14618,   // Amazon AWS (us-east-1 legacy)
    396_982, // Google Cloud
    8075,    // Microsoft Azure
    14061,   // DigitalOcean
    63949,   // Akamai Connected Cloud (Linode)
    24940,   // Hetzner Online
    16276,   // OVH
    20473,   // Vultr (The Constant Company)
    31898,   // Oracle Cloud
    45102,   // Alibaba Cloud
    12876,   // Scaleway
    51167,   // Contabo
    60781,   // Leaseweb
];

/// ASN value iptoasn uses for address space that is not routed.
const NOT_ROUTED_ASN: u32 = 0;

/// An inclusive address range announced by a single ASN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AsnRange<T> {
    start: T,
    end: T,
    asn: u32,
}

/// A row successfully parsed from an ip2asn TSV file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedRow {
    V4(AsnRange<u32>),
    V6(AsnRange<u128>),
}

/// Offline IP → ASN database with a configurable set of "datacenter" ASNs.
///
/// An empty database (the [`Default`]) never matches any IP, so
/// [`is_datacenter`](Self::is_datacenter) always returns `false`.
#[derive(Debug, Clone)]
pub struct AsnDatabase {
    /// IPv4 ranges, sorted by start address.
    v4: Vec<AsnRange<u32>>,
    /// IPv6 ranges, sorted by start address.
    v6: Vec<AsnRange<u128>>,
    /// ASNs considered to be hosting / cloud providers.
    datacenter_asns: HashSet<u32>,
}

impl Default for AsnDatabase {
    fn default() -> Self {
        Self::empty()
    }
}

impl AsnDatabase {
    /// Creates a database with no ranges and the [`DEFAULT_DATACENTER_ASNS`] list.
    pub fn empty() -> Self {
        Self {
            v4: Vec::new(),
            v6: Vec::new(),
            datacenter_asns: DEFAULT_DATACENTER_ASNS.iter().copied().collect(),
        }
    }

    /// Parses ip2asn TSV rows (IPv4, IPv6 or both) from `reader`.
    ///
    /// Blank lines, `#` comments and "Not routed" (`AS_number == 0`) rows are skipped.
    /// Malformed rows are skipped with a warning rather than failing the whole load.
    pub fn from_ip2asn_reader(reader: impl BufRead) -> Result<Self> {
        let mut db = Self::empty();
        db.extend_from_ip2asn_reader(reader)?;
        Ok(db)
    }

    /// Loads a single ip2asn TSV file. See [`from_ip2asn_reader`](Self::from_ip2asn_reader).
    pub fn from_ip2asn_file(path: &Path) -> Result<Self> {
        Self::from_ip2asn_files([path])
    }

    /// Loads and merges several ip2asn TSV files, e.g. the separate v4 and v6 downloads.
    pub fn from_ip2asn_files<I, P>(paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut db = Self::empty();
        for path in paths {
            let path = path.as_ref();
            let file = File::open(path).map_err(|e| io_error(path, &e))?;
            db.extend_from_ip2asn_reader(BufReader::new(file))?;
        }
        Ok(db)
    }

    /// Replaces the set of ASNs treated as datacenters.
    #[must_use]
    pub fn with_datacenter_asns(mut self, asns: impl IntoIterator<Item = u32>) -> Self {
        self.datacenter_asns = asns.into_iter().collect();
        self
    }

    /// Number of IPv4 ranges loaded.
    pub const fn v4_range_count(&self) -> usize {
        self.v4.len()
    }

    /// Number of IPv6 ranges loaded.
    pub const fn v6_range_count(&self) -> usize {
        self.v6.len()
    }

    /// Returns the ASN announcing `ip`, or `None` if no loaded range covers it.
    pub fn lookup_asn(&self, ip: &IpAddr) -> Option<u32> {
        match ip {
            IpAddr::V4(v4) => find_asn(&self.v4, u32::from(*v4)),
            IpAddr::V6(v6) => find_asn(&self.v6, u128::from(*v6)),
        }
    }

    /// Checks whether `ip` belongs to one of the configured datacenter ASNs.
    pub fn is_datacenter(&self, ip: &IpAddr) -> bool {
        self.lookup_asn(ip)
            .is_some_and(|asn| self.datacenter_asns.contains(&asn))
    }

    /// Appends all valid rows from `reader` and restores the sorted-by-start invariant.
    fn extend_from_ip2asn_reader(&mut self, reader: impl BufRead) -> Result<()> {
        for (index, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| DTACError::AsnDbError(e.to_string()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            match parse_ip2asn_row(trimmed) {
                Ok(Some(ParsedRow::V4(range))) => self.v4.push(range),
                Ok(Some(ParsedRow::V6(range))) => self.v6.push(range),
                Ok(None) => {}
                Err(reason) => warn!(line = index + 1, %reason, "Skipping malformed ip2asn row"),
            }
        }
        self.v4.sort_unstable_by_key(|r| r.start);
        self.v6.sort_unstable_by_key(|r| r.start);
        Ok(())
    }
}

/// Reads a list of ASNs, one per line. An optional `AS` prefix is accepted, and blank
/// lines and `#` comments (whole-line or trailing) are ignored.
///
/// Unlike range files, a malformed line here is an error: this list is hand-written
/// configuration, so a typo should fail loudly rather than silently shrink the list.
pub fn read_asn_list(reader: impl BufRead) -> Result<Vec<u32>> {
    let mut asns = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| DTACError::AsnDbError(e.to_string()))?;
        let entry = line.split('#').next().unwrap_or_default().trim();
        if entry.is_empty() {
            continue;
        }
        let digits = entry
            .strip_prefix("AS")
            .or_else(|| entry.strip_prefix("as"))
            .unwrap_or(entry);
        let asn = digits.parse().map_err(|_| {
            DTACError::AsnDbError(format!("line {}: invalid ASN {entry:?}", index + 1))
        })?;
        asns.push(asn);
    }
    Ok(asns)
}

/// Reads an ASN list file. See [`read_asn_list`].
pub fn read_asn_list_file(path: &Path) -> Result<Vec<u32>> {
    let file = File::open(path).map_err(|e| io_error(path, &e))?;
    read_asn_list(BufReader::new(file))
}

fn io_error(path: &Path, err: &std::io::Error) -> DTACError {
    DTACError::AsnDbError(format!("{}: {err}", path.display()))
}

/// Finds the range containing `ip` in `ranges` (sorted by start): the last range whose
/// start is `<= ip`, provided `ip` is also `<=` its end.
fn find_asn<T: Ord + Copy>(ranges: &[AsnRange<T>], ip: T) -> Option<u32> {
    let candidates = ranges.partition_point(|r| r.start <= ip);
    let range = ranges.get(candidates.checked_sub(1)?)?;
    (ip <= range.end).then_some(range.asn)
}

/// Parses one non-comment ip2asn row. Returns `Ok(None)` for "Not routed" rows.
fn parse_ip2asn_row(line: &str) -> std::result::Result<Option<ParsedRow>, String> {
    let mut fields = line.split('\t');
    let mut next_field = |name: &str| fields.next().ok_or_else(|| format!("missing {name}"));

    let start: IpAddr = next_field("range_start")?
        .parse()
        .map_err(|e| format!("bad range_start: {e}"))?;
    let end: IpAddr = next_field("range_end")?
        .parse()
        .map_err(|e| format!("bad range_end: {e}"))?;
    let asn: u32 = next_field("AS_number")?
        .parse()
        .map_err(|e| format!("bad AS_number: {e}"))?;

    if asn == NOT_ROUTED_ASN {
        return Ok(None);
    }

    match (start, end) {
        (IpAddr::V4(s), IpAddr::V4(e)) => AsnRange::new(s.into(), e.into(), asn).map(ParsedRow::V4),
        (IpAddr::V6(s), IpAddr::V6(e)) => AsnRange::new(s.into(), e.into(), asn).map(ParsedRow::V6),
        _ => Err("range_start and range_end are different address families".to_string()),
    }
    .map(Some)
}

impl<T: Ord> AsnRange<T> {
    fn new(start: T, end: T, asn: u32) -> std::result::Result<Self, String> {
        if start > end {
            return Err("range_start is after range_end".to_string());
        }
        Ok(Self { start, end, asn })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const SAMPLE: &str = "\
# comment line
0.0.0.0\t0.255.255.255\t0\tNone\tNot routed

24.0.0.0\t24.15.255.255\t7922\tUS\tCOMCAST-7922
104.248.0.0\t104.248.255.255\t14061\tUS\tDIGITALOCEAN-ASN
104.250.0.0\t104.250.0.255\t7922\tUS\tCOMCAST-7922
2600:3c00::\t2600:3c03:ffff:ffff:ffff:ffff:ffff:ffff\t63949\tUS\tAKAMAI-LINODE-AP
";

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn sample_db() -> AsnDatabase {
        AsnDatabase::from_ip2asn_reader(SAMPLE.as_bytes()).unwrap()
    }

    #[test]
    fn parses_v4_and_v6_rows_and_skips_not_routed() {
        let db = sample_db();
        assert_eq!(db.v4_range_count(), 3);
        assert_eq!(db.v6_range_count(), 1);
        assert_eq!(db.lookup_asn(&ip("0.1.2.3")), None);
    }

    #[test]
    fn lookup_is_inclusive_at_both_ends() {
        let db = sample_db();
        assert_eq!(db.lookup_asn(&ip("104.248.0.0")), Some(14061));
        assert_eq!(db.lookup_asn(&ip("104.248.255.255")), Some(14061));
        assert_eq!(db.lookup_asn(&ip("104.249.0.0")), None);
    }

    #[test]
    fn gap_between_ranges_returns_none() {
        let db = sample_db();
        assert_eq!(db.lookup_asn(&ip("50.0.0.1")), None);
        assert_eq!(db.lookup_asn(&ip("1.1.1.1")), None);
        assert_eq!(db.lookup_asn(&ip("255.255.255.255")), None);
    }

    #[test]
    fn ipv6_lookup_hits() {
        let db = sample_db();
        assert_eq!(db.lookup_asn(&ip("2600:3c01::1")), Some(63949));
        assert!(db.is_datacenter(&ip("2600:3c01::1")));
        assert_eq!(db.lookup_asn(&ip("2600:3c04::1")), None);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let text = "\
not-an-ip\t1.2.3.4\t100\tUS\tBAD
1.0.0.0\t1.0.0.255\tnotanumber\tUS\tBAD
1.0.0.0\t::1\t100\tUS\tMIXED
2.0.0.255\t2.0.0.0\t100\tUS\tREVERSED
3.0.0.0
4.0.0.0\t4.0.0.255\t14061\tUS\tGOOD
";
        let db = AsnDatabase::from_ip2asn_reader(text.as_bytes()).unwrap();
        assert_eq!(db.v4_range_count(), 1);
        assert_eq!(db.v6_range_count(), 0);
        assert!(db.is_datacenter(&ip("4.0.0.7")));
    }

    #[test]
    fn unsorted_input_is_still_found() {
        let text = "\
9.0.0.0\t9.0.0.255\t16509\tUS\tAMAZON
5.0.0.0\t5.0.0.255\t7922\tUS\tCOMCAST
7.0.0.0\t7.0.0.255\t24940\tDE\tHETZNER
";
        let db = AsnDatabase::from_ip2asn_reader(text.as_bytes()).unwrap();
        assert_eq!(db.lookup_asn(&ip("5.0.0.1")), Some(7922));
        assert_eq!(db.lookup_asn(&ip("7.0.0.1")), Some(24940));
        assert_eq!(db.lookup_asn(&ip("9.0.0.1")), Some(16509));
    }

    #[test]
    fn empty_db_is_never_datacenter() {
        let db = AsnDatabase::empty();
        assert!(!db.is_datacenter(&ip("104.248.33.7")));
        assert!(!db.is_datacenter(&ip("2600:3c01::1")));
        assert!(!AsnDatabase::default().is_datacenter(&ip("45.33.12.4")));
    }

    #[test]
    fn default_list_classifies_datacenter_and_isp() {
        let db = sample_db();
        assert!(db.is_datacenter(&ip("104.248.33.7")));
        assert!(!db.is_datacenter(&ip("24.1.2.3")));
    }

    #[test]
    fn custom_datacenter_list_overrides_default() {
        let db = sample_db().with_datacenter_asns([7922]);
        assert!(db.is_datacenter(&ip("24.1.2.3")));
        assert!(!db.is_datacenter(&ip("104.248.33.7")));
    }

    #[test]
    fn asn_list_accepts_prefixes_and_comments() {
        let text = "# hosting\n16509\nAS14061  # DigitalOcean\n\nas24940\n";
        assert_eq!(
            read_asn_list(text.as_bytes()).unwrap(),
            vec![16509, 14061, 24940]
        );
    }

    #[test]
    fn asn_list_rejects_garbage() {
        let err = read_asn_list(&b"16509\nnope\n"[..]).unwrap_err();
        assert!(err.to_string().contains("line 2"));
    }
}
