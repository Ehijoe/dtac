//! Command-line pieces shared by the `dtac_standalone` and `replay_bench` binaries.

use dtac::error::Result;
use dtac::heuristic_router::asn_db::{self, AsnDatabase};
use std::path::PathBuf;

/// ASN database flags, flattened into each binary's `Args`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct AsnArgs {
    /// Path to an iptoasn.com `ip2asn` TSV file (v4, v6 or combined). Repeat the flag to
    /// merge several files, e.g. `--asn-db ip2asn-v4.tsv --asn-db ip2asn-v6.tsv`.
    /// Without it, the datacenter-ASN heuristic never fires.
    #[arg(long = "asn-db", value_name = "PATH")]
    pub asn_db: Vec<PathBuf>,

    /// File listing datacenter ASNs, one per line, replacing the built-in default list.
    #[arg(long, value_name = "PATH", requires = "asn_db")]
    pub datacenter_asns: Option<PathBuf>,
}

impl AsnArgs {
    /// Loads the ASN database described by the flags, or `None` if no `--asn-db` was given.
    pub fn load(&self) -> Result<Option<AsnDatabase>> {
        if self.asn_db.is_empty() {
            return Ok(None);
        }
        let mut db = AsnDatabase::from_ip2asn_files(&self.asn_db)?;
        if let Some(path) = &self.datacenter_asns {
            db = db.with_datacenter_asns(asn_db::read_asn_list_file(path)?);
        }
        Ok(Some(db))
    }
}
