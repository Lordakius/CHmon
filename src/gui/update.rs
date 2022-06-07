use {
    super::{
        Ajour, BackupFolderKind, CatalogCategory, CatalogColumnKey, CatalogRow,
        CatalogSource, ColumnKey, DownloadReason, ExpandType, GlobalReleaseChannel, InstallAddon,
        InstallKind, InstallStatus, Interaction, Message, Mode, ReleaseChannel, SelfUpdateStatus,
        SortDirection, State,
    },
    crate::localization::{localized_string, LANG},
    crate::{log_error, Result},
    ajour_core::{
        addon::{Addon, AddonFolder, AddonState},
        backup::{backup_folders, latest_backup, BackupFolder},
        cache::{
            catalog_download_latest_or_use_cache, remove_addon_cache_entry, update_addon_cache,
            AddonCache, AddonCacheEntry, FingerprintCache,
        },
        catalog,
        config::{ColumnConfigV2, Flavor},
        error::{DownloadError, FilesystemError, ParseError, RepositoryError, ThemeError},
        fs::{delete_addons, delete_saved_variables, import_theme, install_addon, PersistentData},
        network::download_addon,
        parse::{read_addon_directory, update_addon_fingerprint},
        repository::{
            batch_refresh_repository_packages, Changelog, RepositoryKind, RepositoryPackage,
        },
        share,
        utility::{download_update_to_temp_file, get_latest_release, wow_path_resolution},
    },
    ajour_widgets::header::ResizeEvent,
    anyhow::Context,
    async_std::sync::{Arc, Mutex},
    chrono::{NaiveTime, Utc},
    fuzzy_matcher::{
        skim::{SkimMatcherV2, SkimScoreConfig},
        FuzzyMatcher,
    },
    iced::{Command, Length},
    isahc::http::Uri,
    std::collections::{hash_map::DefaultHasher, HashMap},
    std::convert::TryFrom,
    std::hash::Hasher,
    std::path::{Path, PathBuf},
};

use crate::gui::Confirm;
#[cfg(target_os = "windows")]
use crate::tray::{TrayMessage, SHOULD_EXIT, TRAY_SENDER};
#[cfg(target_os = "windows")]
use std::sync::atomic::Ordering;

pub fn handle_message(ajour: &mut Ajour, message: Message) -> Result<Command<Message>> {
    match message {
        Message::CachesLoaded(result) => {
            log::debug!("Message::CachesLoaded(error: {})", result.is_err());

            if let Ok((fingerprint_cache, addon_cache)) = result {
                ajour.fingerprint_cache = Some(Arc::new(Mutex::new(fingerprint_cache)));
                ajour.addon_cache = Some(Arc::new(Mutex::new(addon_cache)));
            }

            return Ok(Command::perform(async {}, Message::Parse));
        }
        Message::Parse(_) => {
            log::debug!("Message::Parse");

            // Begin to parse addon folder(s).
            let mut commands = vec![];

            // If a backup directory is selected, find the latest backup
            if let Some(dir) = &ajour.config.backup_directory {
                commands.push(Command::perform(
                    latest_backup(dir.to_owned()),
                    Message::LatestBackup,
                ));
            }

            // Check if any new flavor has been added since last time.
            // Get missing flavors.
            let mut missing_added = 0;
            let mut missing_flavors: Vec<&Flavor> = vec![];
            for flavor in Flavor::ALL.iter() {
                if ajour.config.wow.directories.get(flavor).is_none() {
                    missing_flavors.push(flavor);
                }
            }

            let flavors = ajour
                .config
                .wow
                .directories
                .keys()
                .copied()
                .collect::<Vec<_>>();
            for flavor in flavors {
                // Find root dir of the flavor and check if any of the missing_flavor's is there.
                // If it is, we added it to the directories.
                if let Some(root_dir) = ajour.config.get_root_directory_for_flavor(&flavor) {
                    for missing_flavor in &missing_flavors {
                        let flavor_dir = ajour
                            .config
                            .get_flavor_directory_for_flavor(missing_flavor, &root_dir);
                        if flavor_dir.exists() {
                            ajour
                                .config
                                .wow
                                .directories
                                .insert(**missing_flavor, flavor_dir);

                            missing_added += 1;
                        }
                    }
                }

                // Check if the current flavor we are looping still exists.
                // It might have been uninstalled since last time, if we can't find it we remove it.
                if let Some(flavor_path) = ajour.config.wow.directories.get(&flavor) {
                    if !flavor_path.exists() {
                        ajour.config.wow.directories.remove(&flavor);
                    }
                }
            }

            // Persist any changes for missing flavors being added
            if missing_added > 0 {
                let _ = ajour.config.save();
            }

            let flavors = ajour.config.wow.directories.keys().collect::<Vec<_>>();
            for flavor in flavors {
                if let Some(addon_directory) = ajour.config.get_addon_directory_for_flavor(flavor) {
                    log::debug!(
                        "preparing to parse addons in {:?}",
                        addon_directory.display()
                    );

                    // Sets loading
                    ajour.state.insert(Mode::MyAddons(*flavor), State::Loading);

                    // Add commands
                    commands.push(Command::perform(
                        perform_read_addon_directory(
                            ajour.addon_cache.clone(),
                            ajour.fingerprint_cache.clone(),
                            addon_directory.clone(),
                            *flavor,
                        ),
                        Message::ParsedAddons,
                    ));

                } else {
                    log::debug!("addon directory is not set, showing welcome screen");
                    break;
                }
            }

            // If we dont have current flavor in valid flavors we select a new.
            let flavor = ajour.config.wow.flavor;
            let flavors = ajour
                .config
                .wow
                .directories
                .keys()
                .collect::<Vec<_>>()
                .clone();
            if !flavors.iter().any(|f| *f == &flavor) {
                if let Some(flavor) = flavors.first() {
                    ajour.config.wow.flavor = **flavor;
                    ajour.mode = Mode::MyAddons(**flavor);
                    ajour.config.save()?;
                }
            }

            return Ok(Command::batch(commands));
        }
        Message::Interaction(Interaction::Refresh(mode)) => {
            log::debug!("Interaction::Refresh({})", &mode);

            // Clear any error message
            ajour.error.take();

            match mode {
                Mode::MyAddons(flavor) => {
                    // Clear query
                    ajour.addons_search_state.query = None;

                    // Close details if shown.
                    ajour.expanded_type = ExpandType::None;

                    // Cleans the addons.
                    ajour.addons = HashMap::new();

                    // Prepare state for loading.
                    ajour.state.insert(Mode::MyAddons(flavor), State::Loading);

                    return Ok(Command::perform(async {}, Message::Parse));
                }
                Mode::Catalog => {
                    ajour.catalog = None;
                    ajour.state.insert(Mode::Catalog, State::Loading);
                    return Ok(Command::perform(
                        catalog_download_latest_or_use_cache(),
                        Message::CatalogDownloaded,
                    ));
                }
                _ => {}
            }
        }
        Message::Interaction(Interaction::Ignore(id)) => {
            log::debug!("Interaction::Ignore({})", &id);

            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            let flavor = ajour.config.wow.flavor;
            let addons = ajour.addons.entry(flavor).or_default();
            let addon = addons.iter_mut().find(|a| a.primary_folder_id == id);

            if let Some(addon) = addon {
                addon.state = AddonState::Ignored;

                // Update the config.
                ajour
                    .config
                    .addons
                    .ignored
                    .entry(flavor)
                    .or_default()
                    .push(addon.primary_folder_id.clone());

                // Persist the newly updated config.
                let _ = &ajour.config.save();
            }
        }
        Message::Interaction(Interaction::Unignore(id)) => {
            log::debug!("Interaction::Unignore({})", &id);

            // Update ajour state.
            let flavor = ajour.config.wow.flavor;
            let global_release_channel = ajour.config.addons.global_release_channel;
            let addons = ajour.addons.entry(flavor).or_default();
            if let Some(addon) = addons.iter_mut().find(|a| a.primary_folder_id == id) {
                // Check if addon is updatable.
                if let Some(package) = addon.relevant_release_package(global_release_channel) {
                    if addon.is_updatable(&package) {
                        addon.state = AddonState::Updatable;
                    } else {
                        addon.state = AddonState::Idle;
                    }
                }
            };

            // Update the config.
            let ignored_addon_ids = ajour.config.addons.ignored.entry(flavor).or_default();
            ignored_addon_ids.retain(|i| i != &id);

            // Persist the newly updated config.
            let _ = &ajour.config.save();
        }
        Message::Interaction(Interaction::OpenDirectory(path)) => {
            log::debug!("Interaction::OpenDirectory({:?})", path);
            let _ = open::that(path);
        }
        Message::Interaction(Interaction::SelectWowDirectory(flavor)) => {
            log::debug!("Interaction::SelectWowDirectory({:?})", flavor);
            return Ok(Command::perform(
                select_wow_directory(flavor),
                Message::UpdateWowDirectory,
            ));
        }
        Message::Interaction(Interaction::SelectBackupDirectory()) => {
            log::debug!("Interaction::SelectBackupDirectory");
            return Ok(Command::perform(
                select_directory(),
                Message::UpdateBackupDirectory,
            ));
        }
        Message::Interaction(Interaction::ResetColumns) => {
            log::debug!("Interaction::ResetColumns");

            ajour.column_settings = Default::default();
            ajour.catalog_column_settings = Default::default();

            ajour.header_state = Default::default();
            ajour.catalog_header_state = Default::default();

            save_column_configs(ajour);
        }
        Message::Interaction(Interaction::OpenLink(link)) => {
            log::debug!("Interaction::OpenLink({})", &link);

            return Ok(Command::perform(
                async {
                    let _ = opener::open(link);
                },
                Message::None,
            ));
        }
        Message::UpdateWowDirectory((chosen_path, flavor)) => {
            log::debug!(
                "Message::UpdateWowDirectory(Chosen({:?} - {:?}))",
                &chosen_path,
                &flavor
            );
            if let Some(path) = wow_path_resolution(chosen_path) {
                log::debug!("Message::UpdateWowDirectory(Resolution({:?}))", &path);
                // Add directories
                ajour.config.add_wow_directories(path, flavor);

                // Clear addons.
                ajour.addons = HashMap::new();

                // Save config.
                let _ = &ajour.config.save();

                for (mode, state) in ajour.state.iter_mut() {
                    if matches!(mode, Mode::MyAddons(_)) {
                        *state = State::Loading;
                    }
                }

                return Ok(Command::perform(async {}, Message::Parse));
            }
        }
        Message::Interaction(Interaction::FlavorSelected(flavor)) => {
            log::debug!("Interaction::FlavorSelected({})", flavor);
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;
            // Update the game flavor
            ajour.config.wow.flavor = flavor;
            // Persist the newly updated config.
            let _ = &ajour.config.save();

            match ajour.mode {
                Mode::MyAddons(_) => {
                    // Update flavor on MyAddons if thats our current mode.
                    ajour.mode = Mode::MyAddons(flavor);
                }
                _ => {}
            }
            // Update catalog
            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::ModeSelected(mode)) => {
            log::debug!("Interaction::ModeSelected({:?})", mode);

            // Remove any pending confirms.
            ajour.pending_confirmation = None;

            // Toggle off About or Settings if button is clicked again
            if ajour.mode == mode && (mode == Mode::About || mode == Mode::Settings) {
                ajour.mode = Mode::MyAddons(ajour.config.wow.flavor);
            }
            // Set mode
            else {
                ajour.mode = mode;
            }
        }

        Message::Interaction(Interaction::Expand(expand_type)) => {
            // Remove any pending confirms.
            ajour.pending_confirmation = None;

            // An addon can be exanded in two ways.
            match &expand_type {
                ExpandType::Details(addon) => {
                    log::debug!(
                        "Interaction::Expand(Details({:?}))",
                        &addon.primary_folder_id
                    );
                    let should_close = match &ajour.expanded_type {
                        ExpandType::Details(a) => addon.primary_folder_id == a.primary_folder_id,
                        ExpandType::Changelog { addon: a, .. } => {
                            addon.primary_folder_id == a.primary_folder_id
                        }
                        _ => false,
                    };

                    if should_close {
                        ajour.expanded_type = ExpandType::None;
                    } else {
                        ajour.expanded_type = expand_type.clone();
                    }
                }
                ExpandType::Changelog { addon, .. } => {
                    log::debug!(
                        "Interaction::Expand(Changelog({:?}))",
                        &addon.primary_folder_id
                    );
                    let should_close = match &ajour.expanded_type {
                        ExpandType::Changelog { addon: a, .. } => {
                            addon.primary_folder_id == a.primary_folder_id
                        }
                        _ => false,
                    };

                    if should_close {
                        ajour.expanded_type = ExpandType::None;
                    } else {
                        ajour.expanded_type = expand_type.clone();

                        return Ok(Command::perform(
                            perform_fetch_changelog(
                                addon.clone(),
                                ajour.config.addons.global_release_channel,
                            ),
                            Message::FetchedChangelog,
                        ));
                    }
                }
                ExpandType::None => {
                    log::debug!("Interaction::Expand(ExpandType::None)");
                }
            }
        }
        Message::FetchedChangelog((addon, result)) => match result {
            Ok(changelog) => {
                log::debug!("Message::FetchedChangelog({})", &addon.primary_folder_id);

                if let ExpandType::Changelog {
                    addon: a,
                    changelog: c,
                } = &mut ajour.expanded_type
                {
                    if a.primary_folder_id == addon.primary_folder_id {
                        *c = Some(changelog);
                    }
                }
            }
            error @ Err(_) => {
                let error = error
                    .context(localized_string("error-fetch-changelog"))
                    .unwrap_err();
                log_error(&error);
                ajour.error = Some(error);
            }
        },
        Message::Interaction(Interaction::DeleteAddon()) => {
            log::debug!("Interaction::DeleteAddon()");
            ajour.pending_confirmation = Some(Confirm::DeleteAddon);
        }
        Message::Interaction(Interaction::ConfirmDeleteAddon(id)) => {
            log::debug!("Interaction::ConfirmDeleteAddon({})", &id);

            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            let flavor = ajour.config.wow.flavor;
            let addons = ajour.addons.entry(flavor).or_default();

            if let Some(addon) = addons.iter().find(|a| a.primary_folder_id == id).cloned() {
                // Remove from local state.
                addons.retain(|a| a.primary_folder_id != addon.primary_folder_id);

                // Delete addon(s) from disk.
                let _ = delete_addons(&addon.folders);

                // Delete SavedVariable(s) if enabled.
                if ajour.config.addons.delete_saved_variables {
                    let wtf_path = &ajour
                        .config
                        .get_wtf_directory_for_flavor(&flavor)
                        .expect("No World of Warcraft directory set.");
                    let _ = delete_saved_variables(&addon.folders, wtf_path);
                }

                // Remove addon from cache
                if let Some(addon_cache) = &ajour.addon_cache {
                    if let Ok(entry) = AddonCacheEntry::try_from(&addon) {
                        match addon.repository_kind() {
                            // Delete the entry for this cached addon
                            Some(RepositoryKind::Tukui)
                            | Some(RepositoryKind::WowI)
                            | Some(RepositoryKind::Git(_)) => {
                                return Ok(Command::perform(
                                    remove_addon_cache_entry(addon_cache.clone(), entry, flavor),
                                    Message::AddonCacheEntryRemoved,
                                ));
                            }
                            _ => {}
                        }
                    }
                }

                // Remove any pending confirms.
                ajour.pending_confirmation = None;
            }
        }
        Message::Interaction(Interaction::DeleteSavedVariables()) => {
            log::debug!("Interaction::DeleteSavedVariables()");
            ajour.pending_confirmation = Some(Confirm::DeleteSavedVariables);
        }
        Message::Interaction(Interaction::ConfirmDeleteSavedVariables(id)) => {
            log::debug!("Interaction::ConfirmDeleteSavedVariables({})", &id);
            let flavor = ajour.config.wow.flavor;
            let addons = ajour.addons.entry(flavor).or_default();

            if let Some(addon) = addons.iter().find(|a| a.primary_folder_id == id).cloned() {
                let wtf_path = &ajour
                    .config
                    .get_wtf_directory_for_flavor(&flavor)
                    .expect("No World of Warcraft directory set.");
                let _ = delete_saved_variables(&addon.folders, wtf_path);
            }

            // Remove any pending confirms.
            ajour.pending_confirmation = None;
            ajour.expanded_type = ExpandType::None;
        }
        Message::Interaction(Interaction::Update(id)) => {
            log::debug!("Interaction::Update({})", &id);

            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            let flavor = ajour.config.wow.flavor;
            let global_release_channel = ajour.config.addons.global_release_channel;
            let addons = ajour.addons.entry(flavor).or_default();
            let to_directory = ajour
                .config
                .get_download_directory_for_flavor(flavor)
                .expect("Expected a valid path");
            for addon in addons.iter_mut() {
                if addon.primary_folder_id == id {
                    addon.state = AddonState::Downloading;
                    return Ok(Command::perform(
                        perform_download_addon(
                            DownloadReason::Update,
                            flavor,
                            global_release_channel,
                            addon.clone(),
                            to_directory,
                        ),
                        Message::DownloadedAddon,
                    ));
                }
            }
        }
        Message::Interaction(Interaction::UpdateAll(mode)) => {
            log::debug!("Interaction::UpdateAll({})", &mode);

            match mode {
                Mode::MyAddons(flavor) => {
                    // Clear query
                    ajour.addons_search_state.query = None;

                    // Close details if shown.
                    ajour.expanded_type = ExpandType::None;

                    // Update all updatable addons, expect ignored.
                    let global_release_channel = ajour.config.addons.global_release_channel;
                    let ignored_ids = ajour.config.addons.ignored.entry(flavor).or_default();
                    let mut addons: Vec<_> = ajour
                        .addons
                        .entry(flavor)
                        .or_default()
                        .iter_mut()
                        .filter(|a| !ignored_ids.iter().any(|i| i == &a.primary_folder_id))
                        .collect();

                    let mut commands = vec![];
                    for addon in addons.iter_mut() {
                        if addon.state == AddonState::Updatable {
                            if let Some(to_directory) =
                                ajour.config.get_download_directory_for_flavor(flavor)
                            {
                                addon.state = AddonState::Downloading;
                                let addon = addon.clone();
                                commands.push(Command::perform(
                                    perform_download_addon(
                                        DownloadReason::Update,
                                        flavor,
                                        global_release_channel,
                                        addon,
                                        to_directory,
                                    ),
                                    Message::DownloadedAddon,
                                ))
                            }
                        }
                    }
                    return Ok(Command::batch(commands));
                }
                _ => {}
            }
        }
        Message::CheckRepositoryUpdates(_) => {
            log::debug!("Message::CheckRepositoryUpdates");

            let mut commands = vec![];

            for flavor in Flavor::ALL.iter() {
                if let Some(addons) = ajour.addons.get(flavor) {
                    let repos = addons
                        .iter()
                        .map(|a| a.repository().cloned())
                        .flatten()
                        .collect::<Vec<_>>();

                    commands.push(Command::perform(
                        perform_batch_refresh_repository_packages(*flavor, repos),
                        Message::RepositoryPackagesFetched,
                    ));
                }
            }

            return Ok(Command::batch(commands));
        }
        Message::RepositoryPackagesFetched((flavor, result)) => {
            match result.context(format!(
                "Failed to fetch repository packages for {}",
                flavor
            )) {
                Ok(packages) => {
                    log::debug!(
                        "Message::RepositoryPackagesFetched({}, {} packages)",
                        flavor,
                        packages.len()
                    );

                    let mut has_update = 0;

                    let addons = ajour.addons.entry(flavor).or_default();
                    let ignored_ids = ajour.config.addons.ignored.entry(flavor).or_default();
                    let global_release_channel = ajour.config.addons.global_release_channel;

                    // For each addon, check if an updated repository package exists. If it does,
                    // we will apply that updated package to the addon, then check if
                    // the addon is updatable.
                    for addon in addons.iter_mut() {
                        // If addon is ignored, we will skip it.
                        if ignored_ids.iter().any(|id| id == &addon.primary_folder_id) {
                            continue;
                        }

                        if let Some(package) = packages.iter().find(|p| {
                            Some(p.id.as_str()) == addon.repository_id()
                                && Some(p.kind) == addon.repository_kind()
                        }) {
                            // Update remote packages from refeshed repository package
                            //
                            // We don't want to replace the entire Repo Package of the addon
                            // because we don't want to modify certain metadata such as File Id,
                            // since we didn't use fingerprints to get these updated packages. We
                            // just want to reference the "latest" remote packages from the repo,
                            // and assign those to the Addon so we can check for new updates
                            addon.set_remote_package_from_repo_package(package);

                            // Check if addon is updatable.
                            if let Some(package) =
                                addon.relevant_release_package(global_release_channel)
                            {
                                if addon.is_updatable(&package) {
                                    log::debug!(
                                        "{} - Update is available for {}, {} -> {}",
                                        flavor,
                                        addon.title(),
                                        addon.version().unwrap_or_default(),
                                        package.version
                                    );

                                    addon.state = AddonState::Updatable;

                                    has_update += 1;
                                }
                            }
                        }
                    }

                    if has_update == 0 {
                        log::debug!("{} - No addon updates available", flavor);
                    } else {
                        // Addons have updates, resort by status to put them up top
                        sort_addons(
                            addons,
                            global_release_channel,
                            SortDirection::Desc,
                            ColumnKey::Status,
                        );
                        ajour.header_state.previous_sort_direction = Some(SortDirection::Desc);
                        ajour.header_state.previous_column_key = Some(ColumnKey::Status);

                        // If auto update is enabled, trigger a refresh all
                        if ajour.config.auto_update {
                            return handle_message(
                                ajour,
                                Message::Interaction(Interaction::UpdateAll(Mode::MyAddons(
                                    flavor,
                                ))),
                            );
                        }
                    }
                }
                Err(error) => {
                    log_error(&error);
                }
            }
        }
        Message::Interaction(Interaction::ToggleAutoUpdateAddons(auto_update)) => {
            log::debug!("Interaction::ToggleAutoUpdateAddons({})", auto_update);

            ajour.config.auto_update = auto_update;
            let _ = ajour.config.save();
        }
        Message::ParsedAddons((flavor, result)) => {
            let global_release_channel = ajour.config.addons.global_release_channel;

            // if our selected flavor returns (either ok or error) - we change to idle.
            ajour.state.insert(Mode::MyAddons(flavor), State::Ready);

            match result.context(localized_string("error-parse-addons")) {
                Ok(addons) => {
                    log::debug!("Message::ParsedAddons({}, {} addons)", flavor, addons.len(),);

                    // Ignored addon ids.
                    let ignored_ids = ajour.config.addons.ignored.entry(flavor).or_default();

                    // Check if addons is updatable.
                    let release_channels = ajour
                        .config
                        .addons
                        .release_channels
                        .entry(flavor)
                        .or_default();
                    let mut addons = addons
                        .into_iter()
                        .map(|mut a| {
                            // Check if we have saved release channel for addon.
                            if let Some(release_channel) =
                                release_channels.get(&a.primary_folder_id)
                            {
                                a.release_channel = *release_channel;
                            } else {
                                // Else we set it to the default release channel.
                                a.release_channel = ReleaseChannel::Default;
                            }

                            // Check if addon is updatable based on release channel.
                            if let Some(package) =
                                a.relevant_release_package(global_release_channel)
                            {
                                if a.is_updatable(&package) {
                                    a.state = AddonState::Updatable;
                                }
                            }

                            if ignored_ids.iter().any(|ia| &a.primary_folder_id == ia) {
                                a.state = AddonState::Ignored;
                            };

                            a
                        })
                        .collect::<Vec<Addon>>();

                    // Sort the addons.
                    sort_addons(
                        &mut addons,
                        global_release_channel,
                        SortDirection::Desc,
                        ColumnKey::Status,
                    );
                    ajour.header_state.previous_sort_direction = Some(SortDirection::Desc);
                    ajour.header_state.previous_column_key = Some(ColumnKey::Status);

                    // Sets the flavor state to ready.
                    ajour.state.insert(Mode::MyAddons(flavor), State::Ready);

                    // Insert the addons into the HashMap.
                    ajour.addons.insert(flavor, addons);

                    // If auto update is enabled, trigger a refresh all
                    if ajour.config.auto_update {
                        return handle_message(
                            ajour,
                            Message::Interaction(Interaction::UpdateAll(Mode::MyAddons(flavor))),
                        );
                    }
                }
                Err(error) => {
                    log_error(&error);
                    ajour
                        .state
                        .insert(Mode::MyAddons(flavor), State::Error(error));
                }
            }
        }
        Message::DownloadedAddon((reason, flavor, id, result)) => {
            log::debug!(
                "Message::DownloadedAddon(({}, {}, error: {}))",
                flavor,
                &id,
                result.is_err()
            );

            let addons = ajour.addons.entry(flavor).or_default();
            let install_addons = ajour.install_addons.entry(flavor).or_default();

            let mut addon = None;

            match result.context(localized_string("error-download-addon")) {
                Ok(_) => match reason {
                    DownloadReason::Update => {
                        if let Some(_addon) = addons.iter_mut().find(|a| a.primary_folder_id == id)
                        {
                            addon = Some(_addon);
                        }
                    }
                    DownloadReason::Install => {
                        if let Some(install_addon) = install_addons
                            .iter_mut()
                            .find(|a| a.addon.as_ref().map(|a| &a.primary_folder_id) == Some(&id))
                        {
                            install_addon.status = InstallStatus::Unpacking;

                            if let Some(_addon) = install_addon.addon.as_mut() {
                                addon = Some(_addon);
                            }
                        }
                    }
                },
                Err(error) => {
                    log_error(&error);
                    ajour.error = Some(error);

                    match reason {
                        DownloadReason::Update => {
                            if let Some(_addon) =
                                addons.iter_mut().find(|a| a.primary_folder_id == id)
                            {
                                _addon.state = AddonState::Retry;
                            }
                        }
                        DownloadReason::Install => {
                            if let Some(install_addon) =
                                install_addons.iter_mut().find(|a| a.id == id)
                            {
                                install_addon.status = InstallStatus::Retry;
                            }
                        }
                    }
                }
            }

            if let Some(addon) = addon {
                let from_directory = ajour
                    .config
                    .get_download_directory_for_flavor(flavor)
                    .expect("Expected a valid path");
                let to_directory = ajour
                    .config
                    .get_addon_directory_for_flavor(&flavor)
                    .expect("Expected a valid path");

                if addon.state == AddonState::Downloading {
                    addon.state = AddonState::Unpacking;

                    return Ok(Command::perform(
                        perform_unpack_addon(
                            reason,
                            flavor,
                            addon.clone(),
                            from_directory,
                            to_directory,
                        ),
                        Message::UnpackedAddon,
                    ));
                }
            }
        }
        Message::UnpackedAddon((reason, flavor, id, result)) => {
            log::debug!(
                "Message::UnpackedAddon(({}, error: {}))",
                &id,
                result.is_err()
            );

            let addons = ajour.addons.entry(flavor).or_default();
            let install_addons = ajour.install_addons.entry(flavor).or_default();

            let mut addon = None;
            let mut folders = None;

            match result.context(localized_string("error-unpack-addon")) {
                Ok(_folders) => match reason {
                    DownloadReason::Update => {
                        if let Some(_addon) = addons.iter_mut().find(|a| a.primary_folder_id == id)
                        {
                            addon = Some(_addon);
                            folders = Some(_folders);
                        }
                    }
                    DownloadReason::Install => {
                        if let Some(install_addon) = install_addons
                            .iter_mut()
                            .find(|a| a.addon.as_ref().map(|a| &a.primary_folder_id) == Some(&id))
                        {
                            if let Some(_addon) = install_addon.addon.as_mut() {
                                // If we are installing from the catalog, remove any existing addon
                                // that has the same folders and insert this new one
                                addons.retain(|a| a.folders != _folders);
                                addons.push(_addon.clone());

                                addon = addons.iter_mut().find(|a| a.primary_folder_id == id);
                                folders = Some(_folders);
                            }
                        }

                        // Remove install addon since we've successfully installed it and
                        // added to main addon vec
                        install_addons.retain(|a| {
                            a.addon.as_ref().map(|a| &a.primary_folder_id) != Some(&id)
                        });
                    }
                },
                Err(error) => {
                    log_error(&error);
                    ajour.error = Some(error);

                    match reason {
                        DownloadReason::Update => {
                            if let Some(_addon) =
                                addons.iter_mut().find(|a| a.primary_folder_id == id)
                            {
                                _addon.state = AddonState::Retry;
                            }
                        }
                        DownloadReason::Install => {
                            if let Some(install_addon) =
                                install_addons.iter_mut().find(|a| a.id == id)
                            {
                                install_addon.status = InstallStatus::Retry;
                            }
                        }
                    }
                }
            }

            let global_release_channel = ajour.config.addons.global_release_channel;
            let mut commands = vec![];

            if let (Some(addon), Some(folders)) = (addon, folders) {
                addon.update_addon_folders(folders);

                addon.state = AddonState::Fingerprint;

                // Set version & file id of installed addon to that of newly unpacked package.
                if let Some(package) = addon.relevant_release_package(global_release_channel) {
                    addon.set_version(package.version);

                    if let Some(file_id) = package.file_id {
                        addon.set_file_id(file_id);
                    }
                }

                // If we are updating / installing a Tukui / WowI / Hub / Git
                // addon, we want to update the cache. If we are installing a Curse
                // addon, we want to make sure cache entry exists for those folders
                if let Some(addon_cache) = &ajour.addon_cache {
                    if let Ok(entry) = AddonCacheEntry::try_from(addon as &_) {
                        match addon.repository_kind() {
                            // Remove any entry related to this cached addon
                            Some(RepositoryKind::Curse) => {
                                commands.push(Command::perform(
                                    remove_addon_cache_entry(addon_cache.clone(), entry, flavor),
                                    Message::AddonCacheEntryRemoved,
                                ));
                            }
                            // Update the entry for this cached addon
                            Some(RepositoryKind::Tukui)
                            | Some(RepositoryKind::WowI)
                            | Some(RepositoryKind::Hub)
                            | Some(RepositoryKind::Git(_)) => {
                                commands.push(Command::perform(
                                    update_addon_cache(addon_cache.clone(), entry, flavor),
                                    Message::AddonCacheUpdated,
                                ));
                            }
                            None => {}
                        }
                    }
                }

                // Submit all addon folders to be fingerprinted
                if let Some(cache) = ajour.fingerprint_cache.as_ref() {
                    for folder in &addon.folders {
                        commands.push(Command::perform(
                            perform_hash_addon(
                                ajour
                                    .config
                                    .get_addon_directory_for_flavor(&flavor)
                                    .expect("Expected a valid path"),
                                folder.id.clone(),
                                cache.clone(),
                                flavor,
                            ),
                            Message::UpdateFingerprint,
                        ));
                    }
                }
            }

            if !commands.is_empty() {
                return Ok(Command::batch(commands));
            }
        }
        Message::UpdateFingerprint((flavor, id, result)) => {
            log::debug!(
                "Message::UpdateFingerprint(({:?}, {}, error: {}))",
                flavor,
                &id,
                result.is_err()
            );

            let addons = ajour.addons.entry(flavor).or_default();
            if let Some(addon) = addons.iter_mut().find(|a| a.primary_folder_id == id) {
                if result.is_ok() {
                    addon.state = AddonState::Completed;
                } else {
                    addon.state = AddonState::Error("Error".to_owned());
                }
            }
        }
        Message::LatestRelease(release) => {
            log::debug!(
                "Message::LatestRelease({:?})",
                release.as_ref().map(|r| &r.tag_name)
            );

            ajour.self_update_state.latest_release = release;
        }
        Message::Interaction(Interaction::SortColumn(column_key)) => {
            // Close details if shown.
            ajour.expanded_type = ExpandType::None;

            // First time clicking a column should sort it in Ascending order, otherwise
            // flip the sort direction.
            let mut sort_direction = SortDirection::Asc;

            if let Some(previous_column_key) = ajour.header_state.previous_column_key {
                if column_key == previous_column_key {
                    if let Some(previous_sort_direction) =
                        ajour.header_state.previous_sort_direction
                    {
                        sort_direction = previous_sort_direction.toggle()
                    }
                }
            }

            // Exception would be first time ever sorting and sorting by title.
            // Since its already sorting in Asc by default, we should sort Desc.
            if ajour.header_state.previous_column_key.is_none() && column_key == ColumnKey::Title {
                sort_direction = SortDirection::Desc;
            }

            log::debug!(
                "Interaction::SortColumn({:?}, {:?})",
                column_key,
                sort_direction
            );

            let flavor = ajour.config.wow.flavor;
            let global_release_channel = ajour.config.addons.global_release_channel;
            let mut addons = ajour.addons.entry(flavor).or_default();

            sort_addons(
                &mut addons,
                global_release_channel,
                sort_direction,
                column_key,
            );

            ajour.header_state.previous_sort_direction = Some(sort_direction);
            ajour.header_state.previous_column_key = Some(column_key);
        }
        Message::Interaction(Interaction::SortCatalogColumn(column_key)) => {
            // First time clicking a column should sort it in Ascending order, otherwise
            // flip the sort direction.
            let mut sort_direction = SortDirection::Asc;

            if let Some(previous_column_key) = ajour.catalog_header_state.previous_column_key {
                if column_key == previous_column_key {
                    if let Some(previous_sort_direction) =
                        ajour.catalog_header_state.previous_sort_direction
                    {
                        sort_direction = previous_sort_direction.toggle()
                    }
                }
            }

            // Exception would be first time ever sorting and sorting by title.
            // Since its already sorting in Asc by default, we should sort Desc.
            if ajour.catalog_header_state.previous_column_key.is_none()
                && column_key == CatalogColumnKey::Title
            {
                sort_direction = SortDirection::Desc;
            }
            // Exception for the date released
            if ajour.catalog_header_state.previous_column_key.is_none()
                && column_key == CatalogColumnKey::DateReleased
            {
                sort_direction = SortDirection::Desc;
            }

            log::debug!(
                "Interaction::SortCatalogColumn({:?}, {:?})",
                column_key,
                sort_direction
            );

            ajour.catalog_header_state.previous_sort_direction = Some(sort_direction);
            ajour.catalog_header_state.previous_column_key = Some(column_key);

            query_and_sort_catalog(ajour);
        }

        Message::ReleaseChannelSelected(release_channel) => {
            log::debug!("Message::ReleaseChannelSelected({:?})", release_channel);

            let global_release_channel = ajour.config.addons.global_release_channel;
            if let ExpandType::Details(expanded_addon) = &ajour.expanded_type {
                let flavor = ajour.config.wow.flavor;
                let addons = ajour.addons.entry(flavor).or_default();
                if let Some(addon) = addons
                    .iter_mut()
                    .find(|a| a.primary_folder_id == expanded_addon.primary_folder_id)
                {
                    // Update config with the newly changed release channel.
                    // if we are selecting Default, we ensure we remove it from config.
                    if release_channel == ReleaseChannel::Default {
                        ajour
                            .config
                            .addons
                            .release_channels
                            .entry(flavor)
                            .or_default()
                            .remove(&addon.primary_folder_id);
                    } else {
                        ajour
                            .config
                            .addons
                            .release_channels
                            .entry(flavor)
                            .or_default()
                            .insert(addon.primary_folder_id.clone(), release_channel);
                    }

                    // Persist the newly updated config.
                    let _ = &ajour.config.save();

                    addon.release_channel = release_channel;

                    // Check if addon is updatable.
                    if let Some(package) = addon.relevant_release_package(global_release_channel) {
                        if addon.is_updatable(&package) {
                            addon.state = AddonState::Updatable;
                        } else {
                            addon.state = AddonState::Idle;
                        }
                    }
                }
            }
        }
        Message::ThemeSelected(theme_name) => {
            log::debug!("Message::ThemeSelected({:?})", &theme_name);

            ajour.theme_state.current_theme_name = theme_name.clone();

            ajour.config.theme = Some(theme_name);
            let _ = ajour.config.save();
        }
        Message::ThemesLoaded(mut themes) => {
            log::debug!("Message::ThemesLoaded({} themes)", themes.len());

            themes.sort();

            for theme in themes {
                ajour.theme_state.themes.push((theme.name.clone(), theme));
            }
        }
        Message::Interaction(Interaction::ResizeColumn(column_type, event)) => match event {
            ResizeEvent::ResizeColumn {
                left_name,
                left_width,
                right_name,
                right_width,
            } => match column_type {
                Mode::MyAddons(_) => {
                    let left_key = ColumnKey::from(left_name.as_str());
                    let right_key = ColumnKey::from(right_name.as_str());

                    if let Some(column) = ajour
                        .header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == left_key && left_key != ColumnKey::Title)
                    {
                        column.width = Length::Units(left_width);
                    }

                    if let Some(column) = ajour
                        .header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == right_key && right_key != ColumnKey::Title)
                    {
                        column.width = Length::Units(right_width);
                    }
                }
                Mode::Install => {}
                Mode::Settings => {}
                Mode::About => {}
                Mode::Catalog => {
                    let left_key = CatalogColumnKey::from(left_name.as_str());
                    let right_key = CatalogColumnKey::from(right_name.as_str());

                    if let Some(column) = ajour
                        .catalog_header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == left_key && left_key != CatalogColumnKey::Title)
                    {
                        column.width = Length::Units(left_width);
                    }

                    if let Some(column) = ajour
                        .catalog_header_state
                        .columns
                        .iter_mut()
                        .find(|c| c.key == right_key && right_key != CatalogColumnKey::Title)
                    {
                        column.width = Length::Units(right_width);
                    }
                }
            },
            ResizeEvent::Finished => {
                // Persist changes to config
                save_column_configs(ajour);
            }
        },
        Message::Interaction(Interaction::ScaleUp) => {
            let prev_scale = ajour.scale_state.scale;

            ajour.scale_state.scale = ((prev_scale + 0.1).min(2.0) * 10.0).round() / 10.0;

            ajour.config.scale = Some(ajour.scale_state.scale);
            let _ = ajour.config.save();

            log::debug!(
                "Interaction::ScaleUp({} -> {})",
                prev_scale,
                ajour.scale_state.scale
            );
        }
        Message::Interaction(Interaction::ScaleDown) => {
            let prev_scale = ajour.scale_state.scale;

            ajour.scale_state.scale = ((prev_scale - 0.1).max(0.5) * 10.0).round() / 10.0;

            ajour.config.scale = Some(ajour.scale_state.scale);
            let _ = ajour.config.save();

            log::debug!(
                "Interaction::ScaleDown({} -> {})",
                prev_scale,
                ajour.scale_state.scale
            );
        }
        Message::UpdateBackupDirectory(path) => {
            log::debug!("Message::UpdateBackupDirectory({:?})", &path);

            if let Some(path) = path {
                // Update the backup directory path.
                ajour.config.backup_directory = Some(path.clone());
                // Persist the newly updated config.
                let _ = &ajour.config.save();

                // Check if a latest backup exists in path
                return Ok(Command::perform(latest_backup(path), Message::LatestBackup));
            }
        }

        Message::Interaction(Interaction::Backup) => {
            log::debug!("Interaction::Backup");

            // This will disable our backup button and show a message that the
            // app is processing the backup. We will unflag this on completion.
            ajour.backup_state.backing_up = true;

            let mut src_folders = vec![];

            // Shouldn't panic since button is only shown if backup directory is chosen
            let dest = ajour.config.backup_directory.as_ref().unwrap();

            // Backup WTF & AddOn directories for flavor if it exist
            for flavor in Flavor::ALL.iter() {
                if let Some(wow_dir) = ajour.config.get_root_directory_for_flavor(flavor) {
                    if ajour.config.backup_addons {
                        let addon_dir =
                            ajour.config.get_addon_directory_for_flavor(flavor).unwrap();

                        // Backup starting with `Interface` folder as some users save
                        // custom data here that they would like retained
                        if let Some(interface_dir) = addon_dir.parent() {
                            if interface_dir.exists() {
                                src_folders.push(BackupFolder::new(interface_dir, &wow_dir));
                            }
                        }
                    }

                    if ajour.config.backup_wtf {
                        let wtf_dir = ajour.config.get_wtf_directory_for_flavor(flavor).unwrap();

                        if wtf_dir.exists() {
                            src_folders.push(BackupFolder::new(&wtf_dir, &wow_dir));
                        }
                    }

                    if ajour.config.backup_screenshots {
                        let screenshot_dir = ajour
                            .config
                            .get_screenshots_directory_for_flavor(flavor)
                            .unwrap();
                        if screenshot_dir.exists() {
                            src_folders.push(BackupFolder::new(&screenshot_dir, &wow_dir));
                        }
                    }

                    if ajour.config.backup_fonts {
                        let fonts_dir =
                            ajour.config.get_fonts_directory_for_flavor(flavor).unwrap();
                        if fonts_dir.exists() {
                            src_folders.push(BackupFolder::new(&fonts_dir, &wow_dir));
                        }
                    }
                }
            }

            // Backup Ajour config.
            if ajour.config.backup_config {
                let config_path = ajour_core::fs::config_dir();
                if let Some(config_prefix) = config_path.parent() {
                    src_folders.push(BackupFolder::new(&config_path, config_prefix));
                }
            }

            return Ok(Command::perform(
                backup_folders(
                    src_folders,
                    dest.to_owned(),
                    ajour.config.compression_format,
                    ajour.config.zstd_compression_level,
                ),
                Message::BackupFinished,
            ));
        }
        Message::Interaction(Interaction::ToggleBackupFolder(is_checked, folder)) => {
            log::debug!(
                "Interaction::ToggleBackupFolder({:?}, checked: {})",
                folder,
                is_checked
            );

            match folder {
                BackupFolderKind::AddOns => {
                    ajour.config.backup_addons = is_checked;
                }
                BackupFolderKind::WTF => {
                    ajour.config.backup_wtf = is_checked;
                }
                BackupFolderKind::Config => {
                    ajour.config.backup_config = is_checked;
                }
                BackupFolderKind::Screenshots => {
                    ajour.config.backup_screenshots = is_checked;
                }
                BackupFolderKind::Fonts => {
                    ajour.config.backup_fonts = is_checked;
                }
            }

            let _ = ajour.config.save();
        }
        Message::LatestBackup(as_of) => {
            log::debug!("Message::LatestBackup({:?})", &as_of);

            ajour.backup_state.last_backup = as_of;
        }
        Message::BackupFinished(Ok(as_of)) => {
            log::debug!("Message::BackupFinished({})", as_of.format("%H:%M:%S"));

            ajour.backup_state.backing_up = false;
            ajour.backup_state.last_backup = Some(as_of);
        }
        Message::BackupFinished(error @ Err(_)) => {
            let error = error
                .context(localized_string("error-backup-folders"))
                .unwrap_err();

            log_error(&error);
            ajour.error = Some(error);

            ajour.backup_state.backing_up = false;
        }
        Message::Interaction(Interaction::ToggleColumn(is_checked, key)) => {
            // We can't untoggle the addon title column
            if key == ColumnKey::Title {
                return Ok(Command::none());
            }

            log::debug!("Interaction::ToggleColumn({}, {:?})", is_checked, key);

            if is_checked {
                if let Some(column) = ajour.header_state.columns.iter_mut().find(|c| c.key == key) {
                    column.hidden = false;
                }
            } else if let Some(column) =
                ajour.header_state.columns.iter_mut().find(|c| c.key == key)
            {
                column.hidden = true;
            }

            // Persist changes to config
            save_column_configs(ajour);
        }
        Message::Interaction(Interaction::MoveColumnLeft(key)) => {
            log::debug!("Interaction::MoveColumnLeft({:?})", key);

            // Update header state ordering and save to config
            if let Some(idx) = ajour.header_state.columns.iter().position(|c| c.key == key) {
                ajour.header_state.columns.swap(idx, idx - 1);

                ajour
                    .header_state
                    .columns
                    .iter_mut()
                    .enumerate()
                    .for_each(|(idx, column)| column.order = idx);

                // Persist changes to config
                save_column_configs(ajour);
            }

            // Update column ordering in settings
            if let Some(idx) = ajour
                .column_settings
                .columns
                .iter()
                .position(|c| c.key == key)
            {
                ajour.column_settings.columns.swap(idx, idx - 1);
            }
        }
        Message::Interaction(Interaction::MoveColumnRight(key)) => {
            log::debug!("Interaction::MoveColumnRight({:?})", key);

            // Update header state ordering and save to config
            if let Some(idx) = ajour.header_state.columns.iter().position(|c| c.key == key) {
                ajour.header_state.columns.swap(idx, idx + 1);

                ajour
                    .header_state
                    .columns
                    .iter_mut()
                    .enumerate()
                    .for_each(|(idx, column)| column.order = idx);

                // Persist changes to config
                save_column_configs(ajour);
            }

            // Update column ordering in settings
            if let Some(idx) = ajour
                .column_settings
                .columns
                .iter()
                .position(|c| c.key == key)
            {
                ajour.column_settings.columns.swap(idx, idx + 1);
            }
        }
        Message::Interaction(Interaction::ToggleCatalogColumn(is_checked, key)) => {
            // We can't untoggle the addon title column
            if key == CatalogColumnKey::Title {
                return Ok(Command::none());
            }

            log::debug!(
                "Interaction::ToggleCatalogColumn({}, {:?})",
                is_checked,
                key
            );

            if is_checked {
                if let Some(column) = ajour
                    .catalog_header_state
                    .columns
                    .iter_mut()
                    .find(|c| c.key == key)
                {
                    column.hidden = false;
                }
            } else if let Some(column) = ajour
                .catalog_header_state
                .columns
                .iter_mut()
                .find(|c| c.key == key)
            {
                column.hidden = true;
            }

            // Persist changes to config
            save_column_configs(ajour);
        }
        Message::Interaction(Interaction::MoveCatalogColumnLeft(key)) => {
            log::debug!("Interaction::MoveCatalogColumnLeft({:?})", key);

            // Update header state ordering and save to config
            if let Some(idx) = ajour
                .catalog_header_state
                .columns
                .iter()
                .position(|c| c.key == key)
            {
                ajour.catalog_header_state.columns.swap(idx, idx - 1);

                ajour
                    .catalog_header_state
                    .columns
                    .iter_mut()
                    .enumerate()
                    .for_each(|(idx, column)| column.order = idx);

                // Persist changes to config
                save_column_configs(ajour);
            }

            // Update column ordering in settings
            if let Some(idx) = ajour
                .catalog_column_settings
                .columns
                .iter()
                .position(|c| c.key == key)
            {
                ajour.catalog_column_settings.columns.swap(idx, idx - 1);
            }
        }
        Message::Interaction(Interaction::MoveCatalogColumnRight(key)) => {
            log::debug!("Interaction::MoveCatalogColumnRight({:?})", key);

            // Update header state ordering and save to config
            if let Some(idx) = ajour
                .catalog_header_state
                .columns
                .iter()
                .position(|c| c.key == key)
            {
                ajour.catalog_header_state.columns.swap(idx, idx + 1);

                ajour
                    .catalog_header_state
                    .columns
                    .iter_mut()
                    .enumerate()
                    .for_each(|(idx, column)| column.order = idx);

                // Persist changes to config
                save_column_configs(ajour);
            }

            // Update column ordering in settings
            if let Some(idx) = ajour
                .catalog_column_settings
                .columns
                .iter()
                .position(|c| c.key == key)
            {
                ajour.catalog_column_settings.columns.swap(idx, idx + 1);
            }
        }
        Message::CatalogDownloaded(Ok(catalog)) => {
            log::debug!(
                "Message::CatalogDownloaded({} addons in catalog)",
                catalog.addons.len()
            );

            ajour.catalog_last_updated = Some(Utc::now());

            let mut categories_per_source =
                catalog
                    .addons
                    .iter()
                    .fold(HashMap::new(), |mut map, addon| {
                        map.entry(addon.source.to_string())
                            .or_insert_with(Vec::new)
                            .append(
                                &mut addon
                                    .categories
                                    .clone()
                                    .iter()
                                    .map(|c| CatalogCategory::Choice(c.to_string()))
                                    .collect(),
                            );
                        map
                    });
            categories_per_source.iter_mut().for_each(move |s| {
                s.1.sort();
                s.1.dedup();
                s.1.insert(0, CatalogCategory::All);
            });

            ajour.catalog_categories_per_source_cache = categories_per_source;
            let catalog_source_choice = ajour
                .config
                .catalog_source
                .map(CatalogSource::Choice)
                .unwrap_or(CatalogSource::All);

            ajour.catalog_search_state.categories = ajour
                .catalog_categories_per_source_cache
                .get(&catalog_source_choice.to_string())
                .cloned()
                .unwrap_or_default();

            ajour.catalog = Some(catalog);

            ajour.state.insert(Mode::Catalog, State::Ready);

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::AddonsQuery(query)) => {
            // Addons search query
            ajour.addons_search_state.query = if query.is_empty() { None } else { Some(query) };

            // Increase penalty for gaps between matching characters
            let fuzzy_match_config = SkimScoreConfig {
                gap_start: -12,
                gap_extension: -6,
                ..Default::default()
            };
            let fuzzy_matcher = SkimMatcherV2::default().score_config(fuzzy_match_config);

            let addons = ajour.addons.entry(ajour.config.wow.flavor).or_default();
            let global_release_channel = ajour.config.addons.global_release_channel;

            if let Some(query) = &ajour.addons_search_state.query {
                addons.iter_mut().for_each(|a| {
                    a.fuzzy_score.take();

                    if let Some(score) = fuzzy_matcher.fuzzy_match(a.title(), query) {
                        if score > 0 {
                            a.fuzzy_score = Some(score);
                        }
                    }
                });

                // Sort the addons by score
                sort_addons(
                    addons,
                    global_release_channel,
                    SortDirection::Desc,
                    ColumnKey::FuzzyScore,
                );
                ajour.header_state.previous_sort_direction = Some(SortDirection::Desc);
                ajour.header_state.previous_column_key = Some(ColumnKey::FuzzyScore);
            } else {
                // Clear out the fuzzy scores
                addons.iter_mut().for_each(|a| {
                    a.fuzzy_score.take();
                });

                // Use default sort
                sort_addons(
                    addons,
                    global_release_channel,
                    SortDirection::Desc,
                    ColumnKey::Status,
                );
                ajour.header_state.previous_sort_direction = Some(SortDirection::Desc);
                ajour.header_state.previous_column_key = Some(ColumnKey::Status);
            }
        }
        Message::Interaction(Interaction::CatalogQuery(query)) => {
            // Catalog search query
            ajour.catalog_search_state.query = if query.is_empty() {
                None
            } else {
                // Always set sort config to None when a new character is typed
                // so the sort will be off fuzzy match score.
                ajour.catalog_header_state.previous_column_key.take();
                ajour.catalog_header_state.previous_sort_direction.take();

                Some(query)
            };

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::InstallAddon(flavor, id, kind)) => {
            log::debug!("Interaction::InstallAddon({}, {:?})", flavor, &kind);

            let install_addons = ajour.install_addons.entry(flavor).or_default();

            // Remove any existing status for this addon since we are going
            // to try and download it again. For InstallKind::Source, we should only
            // ever have one entry here so we just remove it
            install_addons.retain(|a| match kind {
                InstallKind::Catalog { .. } | InstallKind::Import { .. } => {
                    !(id == a.id && a.kind == kind)
                }
                InstallKind::Source => a.kind != kind,
            });

            // Add new status for this addon as Downloading
            install_addons.push(InstallAddon {
                id: id.clone(),
                kind,
                status: InstallStatus::Downloading,
                addon: None,
            });

            return Ok(Command::perform(
                perform_fetch_latest_addon(kind, id, flavor),
                Message::InstallAddonFetched,
            ));
        }
        Message::Interaction(Interaction::CatalogCategorySelected(category)) => {
            log::debug!("Interaction::CatalogCategorySelected({})", &category);

            // Select category
            ajour.catalog_search_state.category = category;

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::CatalogResultSizeSelected(size)) => {
            log::debug!("Interaction::CatalogResultSizeSelected({:?})", &size);

            // Catalog result size
            ajour.catalog_search_state.result_size = size;

            query_and_sort_catalog(ajour);
        }
        Message::Interaction(Interaction::CatalogSourceSelected(source)) => {
            log::debug!("Interaction::CatalogSourceSelected({:?})", source);

            // Save the specific source to the config, otherwise we set `None`
            match source {
                CatalogSource::All => {
                    ajour.config.catalog_source = None;
                }
                CatalogSource::Choice(source) => {
                    ajour.config.catalog_source = Some(source);
                }
            }

            // Save to config
            let _ = ajour.config.save();

            ajour.catalog_search_state.categories = ajour
                .catalog_categories_per_source_cache
                .get(&source.to_string())
                .cloned()
                .unwrap_or_default();

            ajour.catalog_search_state.category = CatalogCategory::All;

            query_and_sort_catalog(ajour);
        }
        Message::InstallAddonFetched((flavor, id, result)) => {
            let install_addons = ajour.install_addons.entry(flavor).or_default();

            if let Some(install_addon) = install_addons.iter_mut().find(|a| a.id == id) {
                match result {
                    Ok(mut addon) => {
                        log::debug!(
                            "Message::CatalogInstallAddonFetched({:?}, {:?})",
                            flavor,
                            &id,
                        );

                        addon.state = AddonState::Downloading;
                        install_addon.addon = Some(addon.clone());

                        let global_release_channel = ajour.config.addons.global_release_channel;
                        let to_directory = ajour
                            .config
                            .get_download_directory_for_flavor(flavor)
                            .expect("Expected a valid path");

                        return Ok(Command::perform(
                            perform_download_addon(
                                DownloadReason::Install,
                                flavor,
                                global_release_channel,
                                addon,
                                to_directory,
                            ),
                            Message::DownloadedAddon,
                        ));
                    }
                    Err(error) => {
                        // Dont use `context` here to convert to anyhow::Error since
                        // we actually want to show the underlying RepositoryError
                        // message
                        let error = anyhow::Error::new(error);

                        log_error(&error);

                        match install_addon.kind {
                            InstallKind::Catalog { .. } => {
                                install_addon.status = InstallStatus::Unavailable;
                            }
                            InstallKind::Source | InstallKind::Import { .. } => {
                                install_addon.status = InstallStatus::Error(error.to_string());
                            }
                        }
                    }
                }
            }
        }
        Message::Interaction(Interaction::UpdateAjour) => {
            log::debug!("Interaction::UpdateAjour");

            if let Some(release) = &ajour.self_update_state.latest_release {
                let bin_name = bin_name().to_owned();

                ajour.self_update_state.status = Some(SelfUpdateStatus::InProgress);

                return Ok(Command::perform(
                    download_update_to_temp_file(bin_name, release.clone()),
                    Message::AjourUpdateDownloaded,
                ));
            }
        }
        Message::AjourUpdateDownloaded(result) => {
            log::debug!("Message::AjourUpdateDownloaded");

            match result.context(localized_string("error-update-ajour")) {
                Ok((relaunch_path, cleanup_path)) => {
                    // Remove first arg, which is path to binary. We don't use this first
                    // arg as binary path because it's not reliable, per the docs.
                    let mut args = std::env::args();
                    args.next();
                    let mut args: Vec<_> = args.collect();

                    // Remove the `--self-update-temp` arg from args if it exists,
                    // since we need to pass it cleanly. Otherwise new process will
                    // fail during arg parsing.
                    if let Some(idx) = args.iter().position(|a| a == "--self-update-temp") {
                        args.remove(idx);
                        // Remove path passed after this arg
                        args.remove(idx);
                    }

                    match std::process::Command::new(&relaunch_path)
                        .args(args)
                        .arg("--self-update-temp")
                        .arg(&cleanup_path)
                        .spawn()
                        .context(localized_string("error-update-ajour"))
                    {
                        Ok(_) => std::process::exit(0),
                        Err(error) => {
                            log_error(&error);
                            ajour.error = Some(error);
                            ajour.self_update_state.status = Some(SelfUpdateStatus::Failed);
                        }
                    }
                }
                Err(mut error) => {
                    // Assign special error message when updating failed due to
                    // permissions issues
                    for cause in error.chain() {
                        if let Some(io_error) = cause.downcast_ref::<std::io::Error>() {
                            if io_error.kind() == std::io::ErrorKind::PermissionDenied {
                                error = error
                                    .context(localized_string("error-update-ajour-permission"));
                                break;
                            }
                        }
                    }

                    log_error(&error);
                    ajour.error = Some(error);
                    ajour.self_update_state.status = Some(SelfUpdateStatus::Failed);
                }
            }
        }
        Message::AddonCacheUpdated(Ok(entry)) => {
            log::debug!("Message::AddonCacheUpdated({})", entry.title);
        }
        Message::AddonCacheEntryRemoved(maybe_entry) => {
            match maybe_entry.context(localized_string("error-remove-cache")) {
                Ok(Some(entry)) => log::debug!("Message::AddonCacheEntryRemoved({})", entry.title),
                Ok(None) => {}
                Err(e) => {
                    log_error(&e);
                }
            }
        }
        Message::Interaction(Interaction::InstallScmQuery(query)) => {
            // install from scm search query
            ajour.install_from_scm_state.query = Some(query);

            // Remove the status if it's an error and user typed into
            // text input
            {
                let install_addons = ajour
                    .install_addons
                    .entry(ajour.config.wow.flavor)
                    .or_default();

                if let Some((idx, install_addon)) = install_addons
                    .iter()
                    .enumerate()
                    .find(|(_, a)| a.kind == InstallKind::Source)
                {
                    if matches!(install_addon.status, InstallStatus::Error(_)) {
                        install_addons.remove(idx);
                    }
                }
            }
        }
        Message::Interaction(Interaction::InstallScmUrl) => {
            if let Some(url) = ajour.install_from_scm_state.query.clone() {
                if !url.is_empty() {
                    return handle_message(
                        ajour,
                        Message::Interaction(Interaction::InstallAddon(
                            ajour.config.wow.flavor,
                            url,
                            InstallKind::Source,
                        )),
                    );
                }
            }
        }
        Message::RefreshCatalog(_) => {
            if let Some(last_updated) = &ajour.catalog_last_updated {
                let now = Utc::now();
                let now_time = now.time();
                let refresh_time = NaiveTime::from_hms(2, 0, 0);

                if last_updated.date() < now.date() && now_time > refresh_time {
                    log::debug!("Message::RefreshCatalog: catalog needs to be refreshed");

                    return Ok(Command::perform(
                        catalog_download_latest_or_use_cache(),
                        Message::CatalogDownloaded,
                    ));
                }
            }
        }
        Message::Interaction(Interaction::ToggleHideIgnoredAddons(is_checked)) => {
            log::debug!("Interaction::ToggleHideIgnoredAddons({})", is_checked);

            ajour.config.hide_ignored_addons = is_checked;
            let _ = ajour.config.save();
        }
        Message::Interaction(Interaction::ToggleDeleteSavedVariables(is_checked)) => {
            log::debug!("Interaction::ToggleDeleteSavedVariables({})", is_checked);

            ajour.config.addons.delete_saved_variables = is_checked;
            let _ = ajour.config.save();
        }
        Message::CatalogDownloaded(error @ Err(_)) => {
            let error = error.context("Failed to download catalog").unwrap_err();
            log_error(&error);
            ajour.state.insert(Mode::Catalog, State::Error(error));
        }
        Message::AddonCacheUpdated(error @ Err(_)) => {
            let error = error.context("Failed to update addon cache").unwrap_err();
            log_error(&error);
            ajour.error = Some(error);
        }
        Message::Interaction(Interaction::PickSelfUpdateChannel(channel)) => {
            log::debug!("Interaction::PickSelfUpdateChannel({:?})", channel);

            ajour.config.self_update_channel = channel;

            let _ = ajour.config.save();

            return Ok(Command::perform(
                get_latest_release(ajour.config.self_update_channel),
                Message::LatestRelease,
            ));
        }
        Message::Interaction(Interaction::PickLocalizationLanguage(lang)) => {
            log::debug!("Interaction::PickLocalizationLanguage({:?})", lang);

            // Update config.
            ajour.config.language = lang;
            let _ = ajour.config.save();

            // Update global LANG refcell.
            *LANG.get().expect("LANG not set").write().unwrap() = lang.language_code();
        }
        Message::Interaction(Interaction::PickGlobalReleaseChannel(channel)) => {
            log::debug!("Interaction::PickGlobalReleaseChannel({:?})", channel);

            // Update all addon states, expect ignored, if needed.
            let flavors = &Flavor::ALL[..];
            for flavor in flavors {
                let ignored_ids = ajour.config.addons.ignored.entry(*flavor).or_default();
                let mut addons: Vec<_> = ajour
                    .addons
                    .entry(*flavor)
                    .or_default()
                    .iter_mut()
                    .filter(|a| !ignored_ids.iter().any(|i| i == &a.primary_folder_id))
                    .collect();
                for addon in addons.iter_mut() {
                    // Check if addon is updatable.
                    if let Some(package) = addon.relevant_release_package(channel) {
                        if addon.is_updatable(&package) {
                            addon.state = AddonState::Updatable;
                        } else {
                            addon.state = AddonState::Idle;
                        }
                    }
                }
            }

            ajour.config.addons.global_release_channel = channel;
            let _ = ajour.config.save();
        }
        Message::CheckLatestRelease(_) => {
            log::debug!("Message::CheckLatestRelease");

            return Ok(Command::perform(
                get_latest_release(ajour.config.self_update_channel),
                Message::LatestRelease,
            ));
        }
        Message::Interaction(Interaction::AlternatingRowColorToggled(is_set)) => {
            log::debug!(
                "Interaction::AlternatingRowColorToggled(is_set: {})",
                is_set,
            );

            ajour.config.alternating_row_colors = is_set;
            let _ = ajour.config.save();
        }
        Message::Interaction(Interaction::KeybindingsToggle(is_set)) => {
            log::debug!("Interaction::KeybindingsToggle(is_set: {})", is_set,);

            ajour.config.is_keybindings_enabled = is_set;
            let _ = ajour.config.save();
        }
        Message::Interaction(Interaction::ExportAddons) => {
            log::debug!("Interaction::ExportAddons");

            return Ok(Command::perform(
                select_export_file(),
                Message::ExportAddons,
            ));
        }
        Message::ExportAddons(path) => {
            if let Some(path) = path {
                log::debug!("Message::ExportAddons({:?})", &path);

                let addons = ajour.addons.clone();

                return Ok(Command::perform(
                    async { share::export(addons, path) },
                    Message::AddonsExported,
                ));
            }
        }
        Message::AddonsExported(result) => match result.context("Failed to export addons") {
            Ok(_) => {
                log::debug!("Message::AddonsExported");
            }
            Err(error) => {
                log_error(&error);

                ajour.error = Some(error);
            }
        },
        Message::Interaction(Interaction::ImportAddons) => {
            log::debug!("Interaction::ImportAddons");

            return Ok(Command::perform(
                select_import_file(),
                Message::ImportAddons,
            ));
        }
        Message::ImportAddons(path) => {
            if let Some(path) = path {
                log::debug!("Message::ImportAddons({:?})", &path);

                let current_addons = ajour.addons.clone();

                ajour.mode = Mode::MyAddons(ajour.config.wow.flavor);

                return Ok(Command::perform(
                    async { share::parse_only_needed(current_addons, path) },
                    Message::ImportParsed,
                ));
            }
        }
        Message::ImportParsed(result) => match result.context("Failed to parse import file") {
            Ok(parsed) => {
                log::debug!("Message::ImportParsed");

                let mut commands = vec![];

                for (flavor, parsed) in parsed.into_iter() {
                    for data in parsed.data {
                        let id = data.id.clone();
                        let repo_kind = data.repo_kind;
                        let install_kind = InstallKind::Import { repo_kind };

                        let command = Command::perform(
                            async move { (flavor, id, install_kind) },
                            |(a, b, c)| Message::Interaction(Interaction::InstallAddon(a, b, c)),
                        );

                        commands.push(command);
                    }
                }

                return Ok(Command::batch(commands));
            }
            Err(error) => {
                log_error(&error);

                ajour.error = Some(error);
            }
        },
        Message::Interaction(Interaction::CompressionLevelChanged(level)) => {
            ajour.config.zstd_compression_level = level;
            let _ = ajour.config.save();
        }
        Message::Error(error) => {
            log_error(&error);
            ajour.error = Some(error);
        }
        Message::RuntimeEvent(iced_native::Event::Window(
            iced_native::window::Event::Resized { width, height },
        )) => {
            let width = (width as f64 * ajour.scale_state.scale) as u32;
            let height = (height as f64 * ajour.scale_state.scale) as u32;

            // Minimizing Ajour on Windows will call this function with 0, 0.
            // We don't want to save that in config, because then it will start with zero size.
            if width > 0 && height > 0 {
                ajour.config.window_size = Some((width, height));
                let _ = ajour.config.save();
            }
        }
        #[cfg(target_os = "windows")]
        Message::RuntimeEvent(iced_native::Event::Window(
            iced_native::window::Event::CloseRequested,
        )) => {
            log::debug!("Message::RuntimeEvent(CloseRequested)");

            if let Some(sender) = TRAY_SENDER.get() {
                if ajour.config.close_to_tray {
                    let _ = sender.try_send(TrayMessage::CloseToTray);
                } else {
                    SHOULD_EXIT.store(true, Ordering::Relaxed);
                }
            }
        }
        Message::RuntimeEvent(iced_native::Event::Keyboard(
            iced_native::keyboard::Event::KeyReleased {
                key_code,
                modifiers,
            },
        )) => {
            // Bail out of keybindings if keybindings is diabled, or we are
            // pressing any modifiers.
            if !ajour.config.is_keybindings_enabled
                || modifiers != iced::keyboard::Modifiers::default()
            {
                return Ok(Command::none());
            }

            match key_code {
                iced::keyboard::KeyCode::A => {
                    let flavor = ajour.config.wow.flavor;
                    ajour.mode = Mode::MyAddons(flavor);
                }
                iced::keyboard::KeyCode::C => {
                    ajour.mode = Mode::Catalog;
                }
                iced::keyboard::KeyCode::R => {
                    let mode = ajour.mode.clone();
                    return handle_message(ajour, Message::Interaction(Interaction::Refresh(mode)));
                }
                iced::keyboard::KeyCode::S => {
                    ajour.mode = Mode::Settings;
                }
                iced::keyboard::KeyCode::U => {
                    let mode = ajour.mode.clone();
                    return handle_message(
                        ajour,
                        Message::Interaction(Interaction::UpdateAll(mode)),
                    );
                }
                iced::keyboard::KeyCode::I => {
                    ajour.mode = Mode::Install;
                }
                iced::keyboard::KeyCode::Escape => match ajour.mode {
                    Mode::Settings | Mode::About => {
                        ajour.mode = Mode::MyAddons(ajour.config.wow.flavor);
                    }
                    Mode::MyAddons(_) => {
                        ajour.addons_search_state.query = None;
                    }
                    Mode::Catalog => {
                        ajour.catalog_search_state.query = None;
                    }
                    _ => (),
                },
                _ => (),
            }
        }
        Message::Interaction(Interaction::PickBackupCompressionFormat(format)) => {
            log::debug!("Interaction::PickBackupCompressionFormat({:?})", format);
            ajour.config.compression_format = format;
            let _ = ajour.config.save();
        }
        #[cfg(target_os = "windows")]
        Message::Interaction(Interaction::ToggleCloseToTray(enable)) => {
            log::debug!("Interaction::ToggleCloseToTray({})", enable);

            ajour.config.close_to_tray = enable;

            // Remove start closed to tray if we are disabling
            if !enable {
                ajour.config.start_closed_to_tray = false;
            }

            let _ = ajour.config.save();

            if let Some(sender) = TRAY_SENDER.get() {
                let msg = if enable {
                    TrayMessage::Enable
                } else {
                    TrayMessage::Disable
                };

                let _ = sender.try_send(msg);
            }
        }
        #[cfg(target_os = "windows")]
        Message::Interaction(Interaction::ToggleAutoStart(enable)) => {
            log::debug!("Interaction::ToggleAutoStart({})", enable);

            ajour.config.autostart = enable;

            let _ = ajour.config.save();

            if let Some(sender) = TRAY_SENDER.get() {
                let _ = sender.try_send(TrayMessage::ToggleAutoStart(enable));
            }
        }
        #[cfg(target_os = "windows")]
        Message::Interaction(Interaction::ToggleStartClosedToTray(enable)) => {
            log::debug!("Interaction::ToggleStartClosedToTray({})", enable);

            ajour.config.start_closed_to_tray = enable;

            // Enable tray if this feature is enabled
            if enable && !ajour.config.close_to_tray {
                ajour.config.close_to_tray = true;

                if let Some(sender) = TRAY_SENDER.get() {
                    let _ = sender.try_send(TrayMessage::Enable);
                }
            }

            let _ = ajour.config.save();
        }
        Message::Interaction(Interaction::ThemeUrlInput(url)) => {
            ajour.theme_state.input_url = url;
        }
        Message::Interaction(Interaction::ImportTheme) => {
            // Reset error
            ajour.error.take();

            let url = ajour.theme_state.input_url.clone();

            log::debug!("Interaction::ImportTheme({})", &url);

            return Ok(Command::perform(import_theme(url), Message::ThemeImported));
        }
        Message::ThemeImported(result) => match result.context("Failed to Import Theme") {
            Ok((new_theme_name, mut new_themes)) => {
                log::debug!("Message::ThemeImported({})", &new_theme_name);

                ajour.theme_state = Default::default();

                new_themes.sort();

                for theme in new_themes {
                    ajour.theme_state.themes.push((theme.name.clone(), theme));
                }

                ajour.theme_state.current_theme_name = new_theme_name.clone();
                ajour.config.theme = Some(new_theme_name);
                let _ = ajour.config.save();
            }
            Err(mut error) => {
                // Reset text input
                ajour.theme_state.input_url = Default::default();
                ajour.theme_state.input_state = Default::default();

                // Assign special error message when updating failed due to
                // collision
                for cause in error.chain() {
                    if let Some(theme_error) = cause.downcast_ref::<ThemeError>() {
                        if matches!(theme_error, ThemeError::NameCollision { .. }) {
                            error = error
                                .context(localized_string("import-theme-error-name-collision"));
                            break;
                        }
                    }
                }

                log_error(&error);
                ajour.error = Some(error);
            }
        },
        Message::RuntimeEvent(_) => {}
        Message::None(_) => {}
    }

    Ok(Command::none())
}

#[cfg(not(target_os = "linux"))]
async fn select_directory() -> Option<PathBuf> {
    use rfd::AsyncFileDialog;

    let dialog = AsyncFileDialog::new();
    if let Some(show) = dialog.pick_folder().await {
        return Some(show.path().to_path_buf());
    }

    None
}

#[cfg(not(target_os = "linux"))]
async fn select_wow_directory(flavor: Option<Flavor>) -> (Option<PathBuf>, Option<Flavor>) {
    use rfd::AsyncFileDialog;

    let dialog = AsyncFileDialog::new();
    if let Some(show) = dialog.pick_folder().await {
        return (Some(show.path().to_path_buf()), flavor);
    }

    (None, flavor)
}

#[cfg(not(target_os = "linux"))]
async fn select_export_file() -> Option<PathBuf> {
    use rfd::AsyncFileDialog;

    let dialog = AsyncFileDialog::new()
        .set_file_name("ajour-addons.yml")
        .add_filter("YML File", &["yml"]);

    dialog.save_file().await.map(|f| f.path().to_path_buf())
}

#[cfg(not(target_os = "linux"))]
async fn select_import_file() -> Option<PathBuf> {
    use rfd::AsyncFileDialog;

    let dialog = AsyncFileDialog::new().add_filter("YML File", &["yml"]);

    dialog.pick_file().await.map(|f| f.path().to_path_buf())
}

#[cfg(target_os = "linux")]
async fn select_directory() -> Option<PathBuf> {
    use native_dialog::FileDialog;

    let dialog = FileDialog::new();
    if let Ok(Some(show)) = dialog.show_open_single_dir() {
        return Some(show);
    }

    None
}

#[cfg(target_os = "linux")]
async fn select_wow_directory(flavor: Option<Flavor>) -> (Option<PathBuf>, Option<Flavor>) {
    use native_dialog::FileDialog;

    let dialog = FileDialog::new();
    if let Ok(Some(show)) = dialog.show_open_single_dir() {
        return (Some(show), flavor);
    }

    (None, flavor)
}

#[cfg(target_os = "linux")]
async fn select_export_file() -> Option<PathBuf> {
    use native_dialog::FileDialog;

    let dialog = FileDialog::new()
        .set_filename("ajour-addons.yml")
        .add_filter("YML File", &["yml"]);

    dialog.show_save_single_file().ok().flatten()
}

#[cfg(target_os = "linux")]
async fn select_import_file() -> Option<PathBuf> {
    use native_dialog::FileDialog;

    let dialog = FileDialog::new().add_filter("YML File", &["yml"]);

    dialog.show_open_single_file().ok().flatten()
}

async fn perform_read_addon_directory(
    addon_cache: Option<Arc<Mutex<AddonCache>>>,
    fingerprint_cache: Option<Arc<Mutex<FingerprintCache>>>,
    root_dir: PathBuf,
    flavor: Flavor,
) -> (Flavor, Result<Vec<Addon>, ParseError>) {
    (
        flavor,
        read_addon_directory(addon_cache, fingerprint_cache, root_dir, flavor).await,
    )
}

/// Downloads the newest version of the addon.
/// This is for now only downloading from warcraftinterface.
async fn perform_download_addon(
    reason: DownloadReason,
    flavor: Flavor,
    global_release_channel: GlobalReleaseChannel,
    addon: Addon,
    to_directory: PathBuf,
) -> (DownloadReason, Flavor, String, Result<(), DownloadError>) {
    (
        reason,
        flavor,
        addon.primary_folder_id.clone(),
        download_addon(&addon, global_release_channel, &to_directory).await,
    )
}

/// Rehashes a `Addon`.
async fn perform_hash_addon(
    addon_dir: impl AsRef<Path>,
    addon_id: String,
    fingerprint_cache: Arc<Mutex<FingerprintCache>>,
    flavor: Flavor,
) -> (Flavor, String, Result<(), ParseError>) {
    (
        flavor,
        addon_id.clone(),
        update_addon_fingerprint(fingerprint_cache, flavor, addon_dir, addon_id).await,
    )
}

/// Unzips `Addon` at given `from_directory` and moves it `to_directory`.
async fn perform_unpack_addon(
    reason: DownloadReason,
    flavor: Flavor,
    addon: Addon,
    from_directory: PathBuf,
    to_directory: PathBuf,
) -> (
    DownloadReason,
    Flavor,
    String,
    Result<Vec<AddonFolder>, FilesystemError>,
) {
    (
        reason,
        flavor,
        addon.primary_folder_id.clone(),
        install_addon(&addon, &from_directory, &to_directory).await,
    )
}

async fn perform_fetch_latest_addon(
    install_kind: InstallKind,
    id: String,
    flavor: Flavor,
) -> (Flavor, String, Result<Addon, RepositoryError>) {
    async fn fetch_latest_addon(
        flavor: Flavor,
        install_kind: InstallKind,
        id: String,
    ) -> Result<Addon, RepositoryError> {
        // Needed since id for source install is a URL and this id needs to be safe
        // when using as the temp path of the downloaded zip
        let mut hasher = DefaultHasher::new();
        hasher.write(format!("{:?}{}", install_kind, &id).as_bytes());
        let temp_id = hasher.finish();

        let mut addon = Addon::empty(&temp_id.to_string());

        let mut repo_package = match install_kind {
            InstallKind::Catalog { source } => {
                let kind = match source {
                    catalog::Source::Curse => RepositoryKind::Curse,
                    catalog::Source::Tukui => RepositoryKind::Tukui,
                    catalog::Source::WowI => RepositoryKind::WowI,
                    catalog::Source::Hub => RepositoryKind::Hub,
                };

                RepositoryPackage::from_repo_id(flavor, kind, id)?
            }
            InstallKind::Source => {
                let url = id
                    .parse::<Uri>()
                    .map_err(|_| RepositoryError::GitInvalidUrl { url: id.clone() })?;

                RepositoryPackage::from_source_url(flavor, url)?
            }
            InstallKind::Import { repo_kind } => {
                RepositoryPackage::from_repo_id(flavor, repo_kind, id)?
            }
        };
        repo_package.resolve_metadata().await?;

        addon.set_repository(repo_package);

        Ok(addon)
    }

    (
        flavor,
        id.clone(),
        fetch_latest_addon(flavor, install_kind, id).await,
    )
}

async fn perform_fetch_changelog(
    addon: Addon,
    default_release_channel: GlobalReleaseChannel,
) -> (Addon, Result<Changelog, RepositoryError>) {
    let changelog = addon.changelog(default_release_channel).await;

    (addon, changelog)
}

async fn perform_batch_refresh_repository_packages(
    flavor: Flavor,
    repos: Vec<RepositoryPackage>,
) -> (Flavor, Result<Vec<RepositoryPackage>, DownloadError>) {
    (
        flavor,
        batch_refresh_repository_packages(flavor, &repos).await,
    )
}

fn sort_addons(
    addons: &mut [Addon],
    global_release_channel: GlobalReleaseChannel,
    sort_direction: SortDirection,
    column_key: ColumnKey,
) {
    match (column_key, sort_direction) {
        (ColumnKey::Title, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.title().to_lowercase().cmp(&b.title().to_lowercase()));
        }
        (ColumnKey::Title, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.title()
                    .to_lowercase()
                    .cmp(&b.title().to_lowercase())
                    .reverse()
                    .then_with(|| {
                        a.relevant_release_package(global_release_channel)
                            .cmp(&b.relevant_release_package(global_release_channel))
                    })
            });
        }
        (ColumnKey::LocalVersion, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.version()
                    .cmp(&b.version())
                    .then_with(|| a.title().cmp(b.title()))
            });
        }
        (ColumnKey::LocalVersion, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.version()
                    .cmp(&b.version())
                    .reverse()
                    .then_with(|| a.title().cmp(b.title()))
            });
        }
        (ColumnKey::RemoteVersion, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package(global_release_channel)
                    .cmp(&b.relevant_release_package(global_release_channel))
                    .then_with(|| a.cmp(b))
            });
        }
        (ColumnKey::RemoteVersion, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package(global_release_channel)
                    .cmp(&b.relevant_release_package(global_release_channel))
                    .reverse()
                    .then_with(|| a.cmp(b))
            });
        }
        (ColumnKey::Status, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.state.cmp(&b.state).then_with(|| a.cmp(b)));
        }
        (ColumnKey::Status, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.state.cmp(&b.state).reverse().then_with(|| a.cmp(b)));
        }
        (ColumnKey::Channel, SortDirection::Asc) => addons.sort_by(|a, b| {
            a.release_channel
                .to_string()
                .cmp(&b.release_channel.to_string())
        }),
        (ColumnKey::Channel, SortDirection::Desc) => addons.sort_by(|a, b| {
            a.release_channel
                .to_string()
                .cmp(&b.release_channel.to_string())
                .reverse()
        }),
        (ColumnKey::Author, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.author().cmp(&b.author()))
        }
        (ColumnKey::Author, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.author().cmp(&b.author()).reverse())
        }
        (ColumnKey::GameVersion, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.game_version().cmp(&b.game_version()))
        }
        (ColumnKey::GameVersion, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.game_version().cmp(&b.game_version()).reverse())
        }
        (ColumnKey::DateReleased, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package(global_release_channel)
                    .map(|p| p.date_time)
                    .cmp(
                        &b.relevant_release_package(global_release_channel)
                            .map(|p| p.date_time),
                    )
            });
        }
        (ColumnKey::DateReleased, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.relevant_release_package(global_release_channel)
                    .map(|p| p.date_time)
                    .cmp(
                        &b.relevant_release_package(global_release_channel)
                            .map(|p| p.date_time),
                    )
                    .reverse()
            });
        }
        (ColumnKey::Source, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.repository_kind().cmp(&b.repository_kind()))
        }
        (ColumnKey::Source, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.repository_kind().cmp(&b.repository_kind()).reverse())
        }
        (ColumnKey::FuzzyScore, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.fuzzy_score.cmp(&b.fuzzy_score))
        }
        (ColumnKey::FuzzyScore, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.fuzzy_score.cmp(&b.fuzzy_score).reverse())
        }
        (ColumnKey::Summary, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.notes().cmp(&b.notes()))
        }
        (ColumnKey::Summary, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.notes().cmp(&b.notes()).reverse())
        }
    }
}

fn sort_catalog_addons(
    addons: &mut [CatalogRow],
    sort_direction: SortDirection,
    column_key: CatalogColumnKey,
    flavor: &Flavor,
) {
    match (column_key, sort_direction) {
        (CatalogColumnKey::Title, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.addon.name.cmp(&b.addon.name));
        }
        (CatalogColumnKey::Title, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.addon.name.cmp(&b.addon.name).reverse());
        }
        (CatalogColumnKey::Description, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.addon.summary.cmp(&b.addon.summary));
        }
        (CatalogColumnKey::Description, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.addon.summary.cmp(&b.addon.summary).reverse());
        }
        (CatalogColumnKey::Source, SortDirection::Asc) => {
            addons.sort_by(|a, b| a.addon.source.cmp(&b.addon.source));
        }
        (CatalogColumnKey::Source, SortDirection::Desc) => {
            addons.sort_by(|a, b| a.addon.source.cmp(&b.addon.source).reverse());
        }
        (CatalogColumnKey::NumDownloads, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.addon
                    .number_of_downloads
                    .cmp(&b.addon.number_of_downloads)
            });
        }
        (CatalogColumnKey::NumDownloads, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.addon
                    .number_of_downloads
                    .cmp(&b.addon.number_of_downloads)
                    .reverse()
            });
        }
        (CatalogColumnKey::Install, SortDirection::Asc) => {}
        (CatalogColumnKey::Install, SortDirection::Desc) => {}
        (CatalogColumnKey::DateReleased, SortDirection::Asc) => addons.sort_by(|a, b| {
            let v_a = a
                .addon
                .versions
                .iter()
                .find(|v| &v.flavor == flavor)
                .map(|v| v.date)
                .flatten();
            let v_b = b
                .addon
                .versions
                .iter()
                .find(|v| &v.flavor == flavor)
                .map(|v| v.date)
                .flatten();
            v_a.cmp(&v_b)
        }),
        (CatalogColumnKey::DateReleased, SortDirection::Desc) => addons.sort_by(|a, b| {
            let v_a = a
                .addon
                .versions
                .iter()
                .find(|v| &v.flavor == flavor)
                .map(|v| v.date)
                .flatten();
            let v_b = b
                .addon
                .versions
                .iter()
                .find(|v| &v.flavor == flavor)
                .map(|v| v.date)
                .flatten();
            v_a.cmp(&v_b).reverse()
        }),
        (CatalogColumnKey::GameVersion, SortDirection::Asc) => addons.sort_by(|a, b| {
            let v_a = a.addon.versions.iter().find(|v| &v.flavor == flavor);
            let v_b = b.addon.versions.iter().find(|v| &v.flavor == flavor);
            v_a.cmp(&v_b)
        }),
        (CatalogColumnKey::GameVersion, SortDirection::Desc) => addons.sort_by(|a, b| {
            let v_a = a.addon.versions.iter().find(|v| &v.flavor == flavor);
            let v_b = b.addon.versions.iter().find(|v| &v.flavor == flavor);
            v_a.cmp(&v_b).reverse()
        }),
        (CatalogColumnKey::Categories, SortDirection::Desc) => {
            addons.sort_by(|a, b| {
                a.addon
                    .categories
                    .join(", ")
                    .cmp(&b.addon.categories.join(", "))
                    .reverse()
            });
        }
        (CatalogColumnKey::Categories, SortDirection::Asc) => {
            addons.sort_by(|a, b| {
                a.addon
                    .categories
                    .join(", ")
                    .cmp(&b.addon.categories.join(", "))
            });
        }
    }
}


fn query_and_sort_catalog(ajour: &mut Ajour) {
    if let Some(catalog) = &ajour.catalog {
        let query = ajour
            .catalog_search_state
            .query
            .as_ref()
            .map(|s| s.to_lowercase());
        let flavor = &ajour.config.wow.flavor;
        let source = &ajour.config.catalog_source;
        let category = &ajour.catalog_search_state.category;
        let result_size = ajour.catalog_search_state.result_size.as_usize();

        // Increase penalty for gaps between matching characters
        let fuzzy_match_config = SkimScoreConfig {
            gap_start: -12,
            gap_extension: -6,
            ..Default::default()
        };
        let fuzzy_matcher = SkimMatcherV2::default().score_config(fuzzy_match_config);

        let mut catalog_rows_and_score = catalog
            .addons
            .iter()
            .filter(|a| !a.versions.is_empty())
            .filter_map(|a| {
                if let Some(query) = &query {
                    let title_score = fuzzy_matcher
                        .fuzzy_match(&a.name, query)
                        .unwrap_or_default();
                    let description_score = fuzzy_matcher
                        .fuzzy_match(&a.summary, query)
                        .unwrap_or_default()
                        / 2;

                    let max_score = title_score.max(description_score);

                    if max_score > 0 {
                        Some((a, max_score))
                    } else {
                        None
                    }
                } else {
                    Some((a, 0))
                }
            })
            .filter(|(a, _)| a.versions.iter().any(|v| v.flavor == flavor.base_flavor()))
            .filter(|(a, _)| match source {
                Some(source) => a.source == *source,
                None => true,
            })
            .filter(|(a, _)| match category {
                CatalogCategory::All => true,
                CatalogCategory::Choice(name) => a.categories.iter().any(|c| c == name),
            })
            .map(|(a, score)| (CatalogRow::from(a.clone()), score))
            .collect::<Vec<(CatalogRow, i64)>>();

        let mut catalog_rows = if query.is_some() {
            // If a query is defined, the default sort is the fuzzy match score
            catalog_rows_and_score.sort_by(|(addon_a, score_a), (addon_b, score_b)| {
                score_a.cmp(score_b).reverse().then_with(|| {
                    addon_a
                        .addon
                        .number_of_downloads
                        .cmp(&addon_b.addon.number_of_downloads)
                        .reverse()
                })
            });

            catalog_rows_and_score
                .into_iter()
                .map(|(a, _)| a)
                .collect::<Vec<_>>()
        } else {
            catalog_rows_and_score
                .into_iter()
                .map(|(a, _)| a)
                .collect::<Vec<_>>()
        };

        // If no query is defined, use the column sorting configuration or default
        // sort of NumDownloads DESC.
        //
        // If a query IS defined, only sort if column has been sorted after query
        // has been typed. Sort direction / key are set to None anytime a character
        // is typed into the query box so results will sort by fuzzy match score.
        // Therefore they'll only be Some if the columns are sorted after the query
        // is input.
        if query.is_none()
            || (ajour.catalog_header_state.previous_sort_direction.is_some()
                && ajour.catalog_header_state.previous_column_key.is_some())
        {
            let sort_direction = ajour
                .catalog_header_state
                .previous_sort_direction
                .unwrap_or(SortDirection::Desc);
            let column_key = ajour
                .catalog_header_state
                .previous_column_key
                .unwrap_or(CatalogColumnKey::NumDownloads);

            sort_catalog_addons(&mut catalog_rows, sort_direction, column_key, flavor);
        }

        catalog_rows = catalog_rows
            .into_iter()
            .enumerate()
            .filter_map(|(idx, row)| if idx < result_size { Some(row) } else { None })
            .collect();

        ajour.catalog_search_state.catalog_rows = catalog_rows;
    }
}

fn save_column_configs(ajour: &mut Ajour) {
    let my_addons_columns: Vec<_> = ajour
        .header_state
        .columns
        .iter()
        .map(ColumnConfigV2::from)
        .collect();

    let catalog_columns: Vec<_> = ajour
        .catalog_header_state
        .columns
        .iter()
        .map(ColumnConfigV2::from)
        .collect();

    let _ = ajour.config.save();
}

/// Hardcoded binary names for each compilation target
/// that gets published to the Github Release
const fn bin_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "ajour.exe"
    }

    #[cfg(target_os = "macos")]
    {
        "ajour"
    }

    #[cfg(target_os = "linux")]
    {
        "ajour.AppImage"
    }
}
