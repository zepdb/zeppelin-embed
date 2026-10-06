pub use zeppelin_embed::format::RegistryError;
#[derive(Clone, Copy)]
pub enum FormatFamily {
    Manifest,
}
impl FormatFamily {
    pub const fn id(self) -> u16 {
        10
    }
    pub const fn current_version(self) -> u16 {
        2
    }
}
pub struct FormatRegistry;
impl FormatRegistry {
    pub fn require(family: u16, version: u16) -> Result<(), RegistryError> {
        if family != 10 {
            return Err(RegistryError::UnknownFamily(family));
        }
        if version != 2 {
            return Err(RegistryError::UnsupportedVersion {
                family,
                version,
                minimum: 2,
                maximum: 2,
            });
        }
        Ok(())
    }
}
pub mod frame;
