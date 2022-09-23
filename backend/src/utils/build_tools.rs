use crate::models::build_tools::{BuildDataInfo, ServerHash};
use crate::models::errors::{BuildToolsError, RepoError, SpigotError};
use crate::models::versions::{SpigotVersion, VersionRefs};
use crate::utils::constants::{PARODY_BUILD_TOOLS_VERSION, SPIGOT_VERSIONS_URL, USER_AGENT};
use crate::utils::net::create_reqwest;
use git2::{Error, ObjectType, Oid, Repository, ResetType};
use log::{debug, info, warn};
use sha1_smol::Sha1;
use std::fs::remove_dir;
use std::io;
use std::io::{copy, Read, Write};
use std::path::{Path, PathBuf};
use tokio::fs::{create_dir, read, remove_file, write, File};
use tokio::task::{spawn_blocking, JoinError, JoinHandle};
use tokio::try_join;

// Example version strings:
// openjdk version "16.0.2" 2021-07-20
// openjdk version "11.0.12" 2021-07-20
// openjdk version "1.8.0_332"

// #[derive(Debug, Clone, PartialEq, Eq)]
// pub struct JavaVersion(pub String);
//
// pub async fn check_java_version() -> Result<JavaVersion, JavaError> {
//     let mut command = Command::new("java");
//     command.args(["-version"]);
//     let output = command.output().await
//         .map_err(|_| JavaError::MissingJava);
//
//
// }

/// Checks if the git repository exists locally on disk and opens it
/// or clones it if it doesn't exist or is invalid.
fn get_repository(url: &str, path: &Path) -> Result<Repository, RepoError> {
    if path.exists() {
        let git_path = path.join(".git");
        let git_exists = git_path.exists();
        if git_exists && git_path.is_dir() {
            match Repository::open(path) {
                Ok(repository) => return Ok(repository),
                Err(_) => {
                    remove_dir(path)?;
                }
            }
        } else if git_exists {
            remove_dir(path)?;
        }
    }
    Ok(Repository::clone(url, path)?)
}

/// Reset the provided repository to the commit that the provided
/// reference is for.
fn reset_to_commit(repo: &Repository, reference: &str) -> Result<(), RepoError> {
    let ref_id = Oid::from_str(reference)?;
    let object = repo.find_object(ref_id, None)?;
    let commit = object.peel(ObjectType::Commit)?;
    repo.reset(&commit, ResetType::Hard, None)?;
    Ok(())
}

#[derive(Debug)]
enum Repo {
    BuildData,
    Spigot,
    Bukkit,
    CraftBukkit,
}

impl Repo {
    pub fn get_repo_url(&self) -> &'static str {
        match self {
            Repo::BuildData => "https://hub.spigotmc.org/stash/scm/spigot/builddata.git",
            Repo::Spigot => "https://hub.spigotmc.org/stash/scm/spigot/spigot.git",
            Repo::Bukkit => "https://hub.spigotmc.org/stash/scm/spigot/bukkit.git",
            Repo::CraftBukkit => "https://hub.spigotmc.org/stash/scm/spigot/craftbukkit.git",
        }
    }

    pub fn get_repo_ref<'a>(&self, refs: &'a VersionRefs) -> &'a str {
        match self {
            Repo::BuildData => &refs.build_data,
            Repo::Spigot => &refs.spigot,
            Repo::Bukkit => &refs.bukkit,
            Repo::CraftBukkit => &refs.craft_bukkit,
        }
    }
}

/// Sets up the provided repo at the provided path resetting its commit to
/// the reference obtained from the `version`
fn setup_repository(
    refs: &VersionRefs,
    repo: Repo,
    path: PathBuf,
) -> JoinHandle<Result<(), RepoError>> {
    let url = repo.get_repo_url();
    let reference = repo
        .get_repo_ref(refs)
        .to_string();
    spawn_blocking(move || {
        let repository = get_repository(url, &path)?;
        reset_to_commit(&repository, &reference)
    })
}

/// Retrieves a spigot version JSON from `SPIGOT_VERSION_URL` and parses it
/// returning the result or a SpigotError
async fn get_spigot_version(version: &str) -> Result<SpigotVersion, SpigotError> {
    let client = create_reqwest()?;
    let url = format!("{}{}.json", SPIGOT_VERSIONS_URL, version);
    let version = client
        .get(url)
        .send()
        .await?
        .json::<SpigotVersion>()
        .await?;
    Ok(version)
}

pub async fn run_build_tools(version: &str) -> Result<(), BuildToolsError> {
    let spigot_version = get_spigot_version(version).await?;
    let build_path = Path::new("build");

    if !build_path.exists() {
        create_dir(build_path).await?;
    }

    setup_repositories(build_path, &spigot_version).await?;
    let build_info = get_build_info(build_path).await?;

    // Check if required version is higher than parody version
    if let Some(tools_version) = build_info.tools_version {
        if tools_version > PARODY_BUILD_TOOLS_VERSION {
            warn!("The build tools version required to build is greater than that which");
            warn!(
                "this tool is able to build (required: {}, parody: {}) ",
                tools_version, PARODY_BUILD_TOOLS_VERSION
            );
        }
    }

    let _ = prepare_vanilla_jar(build_path, &build_info).await?;

    Ok(())
}

/// Sets up the required repositories by downloading them and setting
/// the correct commit ref this is done Asynchronously
pub async fn setup_repositories(
    path: &Path,
    version: &SpigotVersion,
) -> Result<(), BuildToolsError> {
    let refs = &version.refs;

    info!(
        "Setting up repositories in \"{}\" (build_data, bukkit, spigot)",
        path.to_string_lossy()
    );

    // Wait for all the repositories to download and reach the intended reference
    let _ = try_join!(
        setup_repository(refs, Repo::BuildData, path.join("build_data")),
        setup_repository(refs, Repo::Bukkit, path.join("bukkit")),
        setup_repository(refs, Repo::Spigot, path.join("spigot")),
        setup_repository(refs, Repo::CraftBukkit, path.join("craftbukkit")),
    )?;

    Ok(())
}

/// Loads the build_data info configuration
pub async fn get_build_info(path: &Path) -> Result<BuildDataInfo, BuildToolsError> {
    let info_path = path.join("build_data/info.json");
    if !info_path.exists() {
        return Err(BuildToolsError::MissingBuildInfo);
    }
    let info_data = read(info_path).await?;
    let parsed = serde_json::from_slice::<BuildDataInfo>(&info_data)?;
    Ok(parsed)
}

/// Prepares the vanilla jar for decompiling and patching.
/// - Checks the hashes of existing jars
/// - Downloads jar if missing or different hash
/// - Extracts the inner embedded jar if present
/// - Returns the path for the vanilla jar (embedded or not)
async fn prepare_vanilla_jar(
    root: &Path,
    info: &BuildDataInfo,
) -> Result<PathBuf, BuildToolsError> {
    let jar_name = format!("minecraft_server.{}.jar", info.minecraft_version);
    let jar_path = root.join(&jar_name);
    let jar_exists = jar_path.exists();

    if !jar_exists || !check_vanilla_jar(&jar_path, info).await {
        if jar_exists {
            info!(
                "Local hash for jar at \"{}\" didn't match. Re-downloading jar.",
                jar_path.to_string_lossy()
            );
        }
        download_vanilla_jar(&jar_path, info).await?
    } else {
        info!("Hashes match for downloaded jar.")
    }

    let embedded_path = {
        let embedded_name = format!("embedded_server.{}.jar", info.minecraft_version);
        root.join(embedded_name)
    };

    let embedded = extract_embedded(&jar_path, &embedded_path, info).await??;

    if embedded {
        info!("Extracted embedded server jar")
    }

    Ok(if embedded { embedded_path } else { jar_path })
}

/// Attempts to extract the embedded jar from `path` to `embedded_path` but will
/// return whether or not one existed.
fn extract_embedded(
    path: &PathBuf,
    embedded_path: &PathBuf,
    info: &BuildDataInfo,
) -> JoinHandle<Result<bool, BuildToolsError>> {
    use std::fs::{read, write, File};

    let file = File::open(path);
    let embedded_path = embedded_path.clone();

    let embedded_zip_path = format!(
        "META-INF/versions/{0}/server-{0}.jar",
        info.minecraft_version
    );

    let mut existing_hash: Option<String> = None;

    if embedded_path.exists() && embedded_path.is_file() {
        if let Some(mc_hash) = &info.minecraft_hash {
            existing_hash = Some(mc_hash.clone());
        }
    }

    spawn_blocking(move || {
        if let Some(existing_hash) = existing_hash {
            let existing = read(&embedded_path)?;
            let hash = sha256::digest_bytes(&existing);

            info!("Comparing hashes {}, {}", existing_hash, hash);

            if hash.eq(&existing_hash) {
                info!("Already extracted embedded jar with matching hash. Skipping.");
                return Ok(true);
            }
        }

        let file = file?;
        let mut archive = zip::ZipArchive::new(file)?;
        if let Ok(mut embedded) = archive.by_name(&embedded_zip_path) {
            if embedded.is_file() {
                let mut bytes = Vec::with_capacity(embedded.size() as usize);
                embedded.read_to_end(&mut bytes)?;
                write(&embedded_path, bytes)?;
                return Ok(true);
            }
        }
        Ok(false)
    })
}

/// Checks whether the locally stored server jar hash matches the one
/// that we are trying to build. If the hashes don't match or the jar
/// simply doesn't exist then false is returned
async fn check_vanilla_jar(path: &Path, info: &BuildDataInfo) -> bool {
    let info_hash = info.get_server_hash();
    if let Some(info_hash) = info_hash {
        if !path.exists() {
            return false;
        }
        if let Ok(jar_bytes) = read(path).await {
            match info_hash {
                ServerHash::SHA1(hash) => {
                    let mut hasher = Sha1::from(jar_bytes);
                    let result = hasher.digest().to_string();
                    result.eq(hash)
                }
                ServerHash::MD5(hash) => {
                    let result = md5::compute(jar_bytes);
                    let result_hash = format!("{:x}", result);
                    result_hash.eq(hash)
                }
            }
        } else {
            false
        }
    } else {
        path.exists()
    }
}

/// Downloads the vanilla server jar and stores it at
/// the provided path
async fn download_vanilla_jar(path: &Path, info: &BuildDataInfo) -> Result<(), BuildToolsError> {
    let url = info.get_download_url();
    let bytes = reqwest::get(url)
        .await?
        .bytes()
        .await?;
    write(path, bytes).await?;
    Ok(())
}

#[cfg(test)]
mod test {
    use crate::models::build_tools::BuildDataInfo;
    use crate::models::versions::SpigotVersion;
    use crate::utils::build_tools::{run_build_tools, setup_repositories, setup_repository, Repo};
    use crate::utils::constants::{SPIGOT_VERSIONS_URL, USER_AGENT};
    use crate::utils::net::create_reqwest;
    use env_logger::WriteStyle;
    use log::LevelFilter;
    use regex::{Match, Regex};
    use std::fs::{create_dir, read, read_dir};
    use std::path::Path;
    use tokio::fs::write;

    const TEST_VERSIONS: [&str; 12] = [
        "1.8", "1.9", "1.10.2", "1.11", "1.12", "1.13", "1.14", "1.16.1", "1.17", "1.18", "1.19",
        "latest",
    ];

    /// Scrapes the versions externally listed on the index of
    /// https://hub.spigotmc.org/versions/ printing the output
    ///
    /// TODO: Possibly use this as a version list selection?
    /// TODO: or check for checking that spigot has said
    /// TODO: version that is wanting to be downloaded.
    ///
    /// NOTE: Some versions are in the normal format (e.g. 1.8, 1.9)
    /// others are in a different format (e.g. 1023, 1021) when looking
    /// in the 1.8.json, 1.9.json files you will see that the name is in
    /// the 1023, 1021 format which are identical files to the other one.
    #[tokio::test]
    async fn scape_external_version() {
        // User agent is required to access the spigot versions
        // list so this is added here (or else error code: 1020)
        let client = create_reqwest().unwrap();
        let contents = client
            .get(SPIGOT_VERSIONS_URL)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        let regex = Regex::new(r#"<a href="((\d(.)?)+).json">"#).unwrap();

        let values: Vec<&str> = regex
            .captures_iter(&contents)
            .map(|m| m.get(1))
            .filter_map(|m| m)
            .map(|m| m.as_str())
            .collect();

        println!("{:?}", values);
    }

    /// Downloads all the spigot build tools configuration files for the
    /// versions listed at `TEST_VERSIONS` and saves them locally at
    /// test/spigot/{VERSION}.json
    #[tokio::test]
    async fn get_external_versions() {
        // User agent is required to access the spigot versions
        // list so this is added here (or else error code: 1020)
        let client = reqwest::Client::builder()
            .user_agent("Jars/1.0.0")
            .build()
            .unwrap();

        let root_path = Path::new("test/spigot");

        if !root_path.exists() {
            create_dir(root_path).unwrap()
        }

        for version in TEST_VERSIONS {
            let path = root_path.join(format!("{}.json", version));
            let url = format!("{}{}.json", SPIGOT_VERSIONS_URL, version);
            let bytes = client
                .get(url)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            write(path, bytes)
                .await
                .unwrap();
        }
    }

    /// Checks all the JSON files in test/spigot (Only those present in
    /// `TEST_VERSIONS`) to ensure that they are all able to be parsed
    /// without any issues
    #[test]
    fn test_versions() {
        let root_path = Path::new("test/spigot");
        assert!(root_path.exists());
        for version in TEST_VERSIONS {
            let path = root_path.join(format!("{}.json", version));
            assert!(path.exists() && path.is_file());
            let contents = read(path).unwrap();
            let parsed = serde_json::from_slice::<SpigotVersion>(&contents).unwrap();
            println!("{:?}", parsed)
        }
    }

    /// Clones the required repositories for each version pulling the
    /// required reference commit for each different version in
    /// `TEST_VERSIONS`
    #[tokio::test]
    async fn setup_repos() {
        for version in TEST_VERSIONS {
            let version_file = format!("test/spigot/{}.json", version);
            let version_file = Path::new(&version_file);
            let contents = read(version_file).unwrap();
            let parsed = serde_json::from_slice::<SpigotVersion>(&contents).unwrap();

            let test_path = Path::new("test/build");

            setup_repositories(test_path, &parsed)
                .await
                .unwrap();

            let build_data = Path::new("test/build/build_data");
            test_build_data(build_data, version);
        }
    }

    #[tokio::test]
    async fn setup_first_repo() {
        let version = TEST_VERSIONS[0];
        let version_file = format!("test/spigot/{}.json", version);
        let version_file = Path::new(&version_file);
        let contents = read(version_file).unwrap();
        let parsed = serde_json::from_slice::<SpigotVersion>(&contents).unwrap();
        let test_path = Path::new("test/build");
        setup_repositories(test_path, &parsed)
            .await
            .unwrap();
        let build_data = Path::new("test/build/build_data");
        test_build_data(build_data, version);
    }

    #[tokio::test]
    async fn setup_latest() {
        let version = "latest";
        let version_file = format!("test/spigot/{}.json", version);
        let version_file = Path::new(&version_file);
        let contents = read(version_file).unwrap();
        let parsed = serde_json::from_slice::<SpigotVersion>(&contents).unwrap();
        let test_path = Path::new("test/build");
        setup_repositories(test_path, &parsed)
            .await
            .unwrap();
        let build_data = Path::new("test/build/build_data");
        test_build_data(build_data, version);
    }

    /// Tests the build data cloned from the https://hub.spigotmc.org/stash/scm/spigot/builddata.git
    /// repo and ensures that the information in the info.json is both parsable and correct.
    /// (i.e. No files are missing)
    fn test_build_data(path: &Path, version: &str) {
        // Path to info file.
        let info = {
            let path = path.join("info.json");
            let data = read(path).unwrap();
            serde_json::from_slice::<BuildDataInfo>(&data).unwrap()
        };

        if version != "latest" {
            assert_eq!(&info.minecraft_version, version);
        }

        let mappings_path = path.join("mappings");
        let access_transforms = mappings_path.join(info.access_transforms);
        assert!(access_transforms.exists() && access_transforms.is_file());

        let class_mappings = mappings_path.join(info.class_mappings);
        assert!(class_mappings.exists() && class_mappings.is_file());

        if let Some(mm) = info.member_mappings {
            let member_mappings = mappings_path.join(mm);
            assert!(member_mappings.exists() && member_mappings.is_file());
        }

        if let Some(pp) = info.package_mappings {
            let package_mappings = mappings_path.join(pp);
            assert!(package_mappings.exists() && package_mappings.is_file());
        }
    }

    #[tokio::test]
    async fn test_build_tools() {
        dotenv::dotenv().ok();
        env_logger::init();
        run_build_tools("1.19.2")
            .await
            .unwrap();
    }
}

// https://hub.spigotmc.org/stash/scm/spigot/bukkit.git
// https://hub.spigotmc.org/stash/scm/spigot/spigot.git
// https://hub.spigotmc.org/stash/scm/spigot/craftbukkit.git
// https://hub.spigotmc.org/stash/scm/spigot/builddata.git
// https://hub.spigotmc.org/stash/scm/spigot/buildtools.git

// https://hub.spigotmc.org/versions/1.19.2.json
