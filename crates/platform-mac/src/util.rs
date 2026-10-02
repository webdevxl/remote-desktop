use anyhow::{Result, bail};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};

pub(crate) fn dict(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<&CFString> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<&CFType> = pairs.iter().map(|p| p.1).collect();
    CFDictionary::from_slices(&keys, &values)
}

pub(crate) fn check(status: i32, what: &str) -> Result<()> {
    if status != 0 {
        bail!("{what} failed (OSStatus {status})");
    }
    Ok(())
}
