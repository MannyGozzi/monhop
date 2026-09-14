use monhop_transport::identity_store::{StorageError, create_identity, load_identity};
use monhop_transport::native_storage::NativeIdentityStore;

pub fn show() -> Result<(), String> {
    let identity = load_identity(&NativeIdentityStore)
        .map_err(action_error)?
        .ok_or("No local identity exists. Run monhop identity --create to create one. Sharing remains disabled.")?;
    println!(
        "Local identity SHA-256: {}\nPublic fingerprint only. No peer confirmed. Sharing remains disabled.",
        identity.fingerprint()
    );
    Ok(())
}

pub fn create() -> Result<(), String> {
    let identity = create_identity(&NativeIdentityStore).map_err(action_error)?;
    println!(
        "Local identity created and read back from OS-protected storage.\nLocal identity SHA-256: {}\nNo peer confirmed. Sharing remains disabled.",
        identity.fingerprint()
    );
    Ok(())
}

fn action_error(error: StorageError) -> String {
    match error {
        StorageError::AlreadyExists => {
            "A local identity already exists. Run monhop identity --show. No record was replaced."
                .into()
        }
        _ => format!(
            "{error}. No identity was returned. Storage is never automatically repaired or replaced. Sharing remains disabled."
        ),
    }
}
