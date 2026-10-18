//! macOS: the dev cluster in a small Linux VM (Virtualization.framework).

use std::path::Path;

use anyhow::{Result, bail};

pub fn start(_home: &Path, _opts: &super::UpOptions) -> Result<u32> {
    bail!("the macOS dev VM is not built yet")
}

pub fn footprint(_pid: u32) -> Option<u64> {
    None
}
