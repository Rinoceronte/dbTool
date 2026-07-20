use anyhow::Result;
use uuid::Uuid;

const SERVICE: &str = "dbTool";

pub fn set_password(profile_id: Uuid, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, &profile_id.to_string())?;
    entry.set_password(password)?;
    Ok(())
}

pub fn get_password(profile_id: Uuid) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, &profile_id.to_string()).ok()?;
    entry.get_password().ok()
}

pub fn delete_password(profile_id: Uuid) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, &profile_id.to_string())?;
    let _ = entry.delete_credential();
    Ok(())
}
