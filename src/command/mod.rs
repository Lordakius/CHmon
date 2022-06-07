use super::Result;

mod backup;
pub use backup::backup;

mod install;
pub use install::install_from_source;

mod update_addons;
pub use update_addons::update_all_addons;

mod paths;
pub use paths::path_add;

pub fn update_both() -> Result<()> {
    update_all_addons()?;

    Ok(())
}
