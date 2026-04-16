use std::path::{Path, PathBuf};

use solana_pubkey::Pubkey;

use crate::{
    cheatcodes::read_keypair_pubkey,
    error::{SurfnetError, SurfnetResult},
};

/// Builder for deploying a program to Surfnet.
///
/// Unlike single-RPC cheatcode builders, deployment is a compound operation:
/// it writes the program bytes first and then optionally registers an IDL.
///
/// ```rust
/// use surfpool_sdk::{Pubkey, Surfnet};
/// use surfpool_sdk::cheatcodes::builders::DeployProgram;
///
/// # async fn example() {
/// let surfnet = Surfnet::start().await.unwrap();
/// let cheats = surfnet.cheatcodes();
/// let program_id = Pubkey::new_unique();
///
/// cheats
///     .deploy(
///         DeployProgram::new(program_id)
///             .so_path("target/deploy/my_program.so")
///             .idl_path("target/idl/my_program.json"),
///     )
///     .unwrap();
/// # }
/// ```
pub struct DeployProgram {
    program_id: Pubkey,
    so_path: Option<PathBuf>,
    so_bytes: Option<Vec<u8>>,
    idl_path: Option<PathBuf>,
}

impl DeployProgram {
    /// Create a deployment builder from a known program id.
    pub fn new(program_id: Pubkey) -> Self {
        Self {
            program_id,
            so_path: None,
            so_bytes: None,
            idl_path: None,
        }
    }

    /// Create a deployment builder from a Solana keypair file.
    ///
    /// The program id is derived from the keypair public key.
    pub fn from_keypair_path(path: impl AsRef<Path>) -> SurfnetResult<Self> {
        let path = path.as_ref();
        let program_id = read_keypair_pubkey(path)?;
        Ok(Self::new(program_id))
    }

    /// Set the path to the `.so` artifact to deploy.
    pub fn so_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.so_path = Some(path.into());
        self
    }

    /// Set the raw `.so` bytes directly.
    pub fn so_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.so_bytes = Some(bytes);
        self
    }

    /// Set the path to an Anchor IDL JSON file to register after deployment.
    pub fn idl_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.idl_path = Some(path.into());
        self
    }

    /// Set the IDL path only if the file exists.
    pub(crate) fn idl_path_if_exists(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if path.exists() {
            self.idl_path = Some(path);
        }
        self
    }

    /// Return the program id that will be deployed.
    pub(crate) fn program_id(&self) -> Pubkey {
        self.program_id
    }

    /// Resolve the program bytes from either an explicit path or inline bytes.
    pub(crate) fn load_so_bytes(&self) -> SurfnetResult<Vec<u8>> {
        match (&self.so_bytes, &self.so_path) {
            (Some(bytes), _) => Ok(bytes.clone()),
            (None, Some(path)) => std::fs::read(path).map_err(|e| {
                SurfnetError::Cheatcode(format!(
                    "failed to read program bytes from {}: {e}",
                    path.display()
                ))
            }),
            (None, None) => Err(SurfnetError::Cheatcode(
                "deploy program requires either so_path or so_bytes".to_string(),
            )),
        }
    }

    /// Resolve and parse the optional IDL file.
    pub(crate) fn load_idl(&self) -> SurfnetResult<Option<surfpool_types::Idl>> {
        let Some(path) = &self.idl_path else {
            return Ok(None);
        };

        let contents = std::fs::read_to_string(path).map_err(|e| {
            SurfnetError::Cheatcode(format!("failed to read IDL from {}: {e}", path.display()))
        })?;
        let idl = serde_json::from_str(&contents).map_err(|e| {
            SurfnetError::Cheatcode(format!("failed to parse IDL from {}: {e}", path.display()))
        })?;
        Ok(Some(idl))
    }
}
