#[allow(clippy::module_inception)]
#[cfg(feature = "pipes")]
mod pipes {
    use regex::Regex;
    use std::path::PathBuf;
    use tokio::process::Command;

    use reqwest::Client;
    use serde_json::Value;

    use anyhow::Result;
    use reqwest;
    use std::fs;
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tracing::{debug, error, info};
    use url::Url;
    use which::which;

    // Update this function near the top of the file
    fn sanitize_pipe_name(name: &str) -> String {
        let re = Regex::new(r"[^a-zA-Z0-9_-]").unwrap();
        let sanitized = re.replace_all(name, "-").to_string();

        // Remove "-ref-main/" suffix if it exists
        sanitized
            .strip_suffix("-ref-main/")
            .or_else(|| sanitized.strip_suffix("-ref-main"))
            .unwrap_or(&sanitized)
            .to_string()
    }

    pub async fn run_pipe(pipe: &str, screenpipe_dir: PathBuf) -> Result<()> {
        let pipe_dir = screenpipe_dir.join("pipes").join(pipe);
        let main_module = find_pipe_file(&pipe_dir)?;

        info!("executing pipe: {:?}", main_module);

        // Prepare environment variables
        let mut env_vars = std::env::vars().collect::<Vec<(String, String)>>();
        env_vars.push((
            "SCREENPIPE_DIR".to_string(),
            screenpipe_dir.to_str().unwrap().to_string(),
        ));
        env_vars.push(("PIPE_ID".to_string(), pipe.to_string()));
        env_vars.push((
            "PIPE_FILE".to_string(),
            main_module.to_str().unwrap().to_string(),
        ));
        env_vars.push((
            "PIPE_DIR".to_string(),
            pipe_dir.to_str().unwrap().to_string(),
        ));

        // Execute Deno
        let child_result = Command::new("deno")
            .arg("run")
            .arg("--config")
            .arg(pipe_dir.join("deno.json"))
            .arg("--allow-read")
            .arg("--allow-write")
            .arg("--allow-net")
            .arg("--allow-env")
            .arg("--reload")
            .arg(&main_module)
            .envs(env_vars)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let mut child = match child_result {
            Ok(child) => child,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::bail!("deno not found in system path. please install deno: https://deno.land/#installation");
                } else {
                    anyhow::bail!("failed to spawn deno process: {}", e);
                }
            }
        };

        let stdout = child.stdout.take().expect("failed to get stdout");
        let stderr = child.stderr.take().expect("failed to get stderr");

        let pipe_clone = pipe.to_string();

        // Spawn tasks to handle stdout and stderr
        let stdout_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(line) = lines.next_line().await {
                if let Some(line) = line {
                    info!("[pipe][info][{}] {}", pipe_clone, line);
                }
            }
        });

        let pipe_clone = pipe.to_string();

        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(line) = lines.next_line().await {
                if let Some(line) = line {
                    if line.contains("Download") {
                        // Log download messages as info instead of error
                        debug!("[pipe][download][{}] {}", pipe_clone, line);
                    } else {
                        // Keep other messages as errors
                        error!("[pipe][error][{}] {}", pipe_clone, line);
                    }
                }
            }
        });

        // Wait for the child process to finish
        let status = child.wait().await?;

        // Wait for the output handling tasks to finish
        stdout_handle.await?;
        stderr_handle.await?;

        if !status.success() {
            anyhow::bail!("deno execution failed with status: {}", status);
        }

        info!("deno execution completed successfully");
        Ok(())
    }

    pub async fn download_pipe(source: &str, screenpipe_dir: PathBuf) -> anyhow::Result<PathBuf> {
        info!("processing pipe from source: {}", source);

        let pipe_name =
            sanitize_pipe_name(Path::new(source).file_name().unwrap().to_str().unwrap());
        let dest_dir = screenpipe_dir.join("pipes").join(&pipe_name);

        // if dest_dir.exists() {
        //     info!("pipe already exists: {:?}", dest_dir);
        //     return Ok(dest_dir);
        // }
        // TODO

        tokio::fs::create_dir_all(&dest_dir).await?;

        if let Ok(parsed_url) = Url::parse(source) {
            if parsed_url.host_str() == Some("github.com") {
                download_github_folder(&parsed_url, &dest_dir).await?;
            } else {
                anyhow::bail!("unsupported url format");
            }
        } else {
            let source_path = Path::new(source);
            if !source_path.exists() {
                anyhow::bail!("local source path does not exist");
            }
            if !source_path.is_dir() {
                anyhow::bail!("local source is not a directory");
            }

            copy_dir_all(source_path, &dest_dir).await?;
            info!("Copied local folder: {:?} to {:?}", source_path, dest_dir);
        }

        info!("pipe copied successfully to: {:?}", dest_dir);
        Ok(dest_dir)
    }

    async fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&dst).await?;
        let mut entries = tokio::fs::read_dir(src).await?;

        while let Some(entry) = entries.next_entry().await? {
            let ty = entry.file_type().await?;
            let src_path = entry.path();
            let file_name = entry.file_name();

            if !is_hidden_file(&file_name) {
                let dst_path = dst.as_ref().join(&file_name);

                if ty.is_dir() {
                    copy_dir_all_boxed(src_path, dst_path).await?;
                } else {
                    tokio::fs::copy(src_path, dst_path).await?;
                }
            } else {
                info!("skipping hidden file: {:?}", file_name);
            }
        }

        Ok(())
    }

    fn copy_dir_all_boxed(
        src: impl AsRef<Path> + Send + Sync + 'static,
        dst: impl AsRef<Path> + Send + Sync + 'static,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync>> {
        Box::pin(async move { copy_dir_all(src, dst).await })
    }

    async fn download_github_folder(url: &Url, dest_dir: &Path) -> anyhow::Result<()> {
        let client = Client::new();
        let api_url = get_raw_github_url(url.as_str())?;

        let response = client
            .get(&api_url)
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "screenpipe")
            .send()
            .await?;

        let contents: Value = response.json().await?;

        if !contents.is_array() {
            anyhow::bail!("invalid response from github api");
        }

        for item in contents.as_array().unwrap() {
            let file_name = item["name"].as_str().unwrap();
            if !is_hidden_file(std::ffi::OsStr::new(file_name)) {
                let download_url = item["download_url"].as_str().unwrap();
                let file_content = client.get(download_url).send().await?.bytes().await?;
                let file_path = dest_dir.join(file_name);
                tokio::fs::write(&file_path, &file_content).await?;
                info!("downloaded: {:?}", file_path);
            } else {
                info!("skipping hidden file: {}", file_name);
            }
        }

        Ok(())
    }

    fn get_raw_github_url(url: &str) -> anyhow::Result<String> {
        info!("Attempting to get raw GitHub URL for: {}", url);
        let parsed_url = Url::parse(url)?;
        if parsed_url.host_str() == Some("github.com") {
            let path_segments: Vec<&str> = parsed_url.path_segments().unwrap().collect();
            if path_segments.len() >= 5 && path_segments[2] == "tree" {
                let (owner, repo, _, branch) = (
                    path_segments[0],
                    path_segments[1],
                    path_segments[2],
                    path_segments[3],
                );
                let raw_path = path_segments[4..].join("/");
                let raw_url = format!(
                    "https://api.github.com/repos/{}/{}/contents/{}?ref={}",
                    owner, repo, raw_path, branch
                );
                info!("Converted to GitHub API URL: {}", raw_url);
                return Ok(raw_url);
            }
        }
        anyhow::bail!("Invalid GitHub URL format")
    }

    fn find_pipe_file(pipe_dir: &Path) -> anyhow::Result<PathBuf> {
        for entry in fs::read_dir(pipe_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let file_name_str = file_name.to_str().unwrap();
            if (file_name_str == "pipe.js" || file_name_str == "pipe.ts")
                && !is_hidden_file(&file_name)
            {
                return Ok(entry.path());
            }
        }
        anyhow::bail!("No pipe.js/pipe.ts found in the pipe/dist directory")
    }

    fn is_hidden_file(file_name: &std::ffi::OsStr) -> bool {
        file_name
            .to_str()
            .map(|s| s.starts_with('.') || s == "Thumbs.db")
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    const DENO_EXECUTABLE_NAME: &str = "deno";

    #[cfg(windows)]
    const DENO_EXECUTABLE_NAME: &str = "deno.exe";

    pub fn find_deno() -> Option<PathBuf> {
        debug!("starting search for deno executable");

        // check if `deno` is in the PATH environment variable
        if let Ok(path) = which(DENO_EXECUTABLE_NAME) {
            debug!("found deno in PATH: {:?}", path);
            return Some(path);
        }
        debug!("deno not found in PATH");

        // check in current working directory
        if let Ok(cwd) = std::env::current_dir() {
            debug!("current working directory: {:?}", cwd);
            let deno_in_cwd = cwd.join(DENO_EXECUTABLE_NAME);
            if deno_in_cwd.is_file() && deno_in_cwd.exists() {
                debug!("found deno in current working directory: {:?}", deno_in_cwd);
                return Some(deno_in_cwd);
            }
            debug!("deno not found in current working directory");
        }

        // check in the same folder as the executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_folder) = exe_path.parent() {
                debug!("executable folder: {:?}", exe_folder);
                let deno_in_exe_folder = exe_folder.join(DENO_EXECUTABLE_NAME);
                if deno_in_exe_folder.exists() {
                    debug!("found deno in executable folder: {:?}", deno_in_exe_folder);
                    return Some(deno_in_exe_folder);
                }
                debug!("deno not found in executable folder");

                // platform-specific checks
                #[cfg(target_os = "macos")]
                {
                    let resources_folder = exe_folder.join("../Resources");
                    debug!("resources folder: {:?}", resources_folder);
                    let deno_in_resources = resources_folder.join(DENO_EXECUTABLE_NAME);
                    if deno_in_resources.exists() {
                        debug!("found deno in resources folder: {:?}", deno_in_resources);
                        return Some(deno_in_resources);
                    }
                    debug!("deno not found in resources folder");
                }

                #[cfg(target_os = "linux")]
                {
                    let lib_folder = exe_folder.join("lib");
                    debug!("lib folder: {:?}", lib_folder);
                    let deno_in_lib = lib_folder.join(DENO_EXECUTABLE_NAME);
                    if deno_in_lib.exists() {
                        debug!("found deno in lib folder: {:?}", deno_in_lib);
                        return Some(deno_in_lib);
                    }
                    debug!("deno not found in lib folder");
                }
            }
        }

        error!("deno not found");
        None // return None if deno is not found
    }
}

#[cfg(feature = "pipes")]
pub use pipes::*;
