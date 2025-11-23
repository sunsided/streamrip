use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use async_recursion::async_recursion;
use clap::Parser;
use pathdiff::diff_paths;
use reqwest::Client;
use tokio::io::AsyncWriteExt;
use url::Url;

#[derive(Parser, Debug)]
#[command(about = "Recursively mirror an HLS stream and rewrite manifests for local hosting")]
struct Args {
    /// Starting manifest URL (usually a master manifest.m3u8)
    #[arg(short, long)]
    start_url: String,

    /// Output directory to mirror into
    #[arg(short, long)]
    output_dir: PathBuf,
}

struct Mirror {
    client: Client,
    out_dir: PathBuf,
    visited: HashSet<Url>,
    master_url_path_components: Vec<String>,
    url_to_path: HashMap<Url, PathBuf>,
}

impl Mirror {
    fn new(out_dir: PathBuf, master_url_path_components: Vec<String>) -> Self {
        let client = Client::builder()
            .user_agent("hls-mirror/0.1")
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            out_dir,
            visited: HashSet::new(),
            master_url_path_components,
            url_to_path: HashMap::new(),
        }
    }

    /// Decide the local path for a URL, possibly renaming if it has a query string.
    fn path_for_url(&mut self, url: &Url, is_manifest: bool) -> PathBuf {
        if let Some(existing) = self.url_to_path.get(url) {
            return existing.clone();
        }

        // --- NEW LOGIC: compute relative path based on master manifest base ---
        // Example:
        // master: https://server/x/y/z/manifest.m3u8
        // child:  https://server/x/y/z/sub/foo.m3u8
        //
        // rel = "sub/foo.m3u8"
        //
        // Local file = out_dir/rel
        //
        let rel = url
            .path()
            .trim_start_matches('/')
            .split('/')
            .collect::<Vec<_>>();

        let base = self.master_url_path_components.as_slice();

        // Find the relative difference:
        // e.g. master = ["x","y","z","manifest.m3u8"]
        //      child  = ["x","y","z","sub","foo.m3u8"]
        // rel_parts = ["sub","foo.m3u8"]
        let mut idx = 0;
        while idx < base.len() - 1 && idx < rel.len() - 1 && base[idx] == rel[idx] {
            idx += 1;
        }

        let rel_parts = rel[idx..].join("/");

        // Local path under output dir:
        let mut local_path = self.out_dir.join(rel_parts);

        // Ensure manifest has a .m3u8 extension
        if is_manifest && local_path.extension().is_none() {
            local_path.set_extension("m3u8");
        }

        // Handle query string → safe filenames
        if let Some(q) = url.query() {
            let fname = local_path.file_name().unwrap().to_string_lossy();
            let (stem, ext) = fname
                .rsplit_once('.')
                .map(|(s, e)| (s.to_string(), Some(e.to_string())))
                .unwrap_or((fname.to_string(), None));

            let mut safe: String = q
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            if safe.len() > 32 {
                safe.truncate(32);
            }

            let new_name = match ext {
                Some(ext) => format!("{stem}__q_{safe}.{ext}"),
                None => format!("{stem}__q_{safe}"),
            };

            local_path.set_file_name(new_name);
        }

        self.url_to_path.insert(url.clone(), local_path.clone());
        local_path
    }

    fn to_posix_relative(target: &Path, base: &Path) -> String {
        let rel = diff_paths(target, base).unwrap_or_else(|| target.to_path_buf());
        // Join components with '/' for HLS manifests
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        parts.join("/")
    }

    fn find_uri_attr(line: &str) -> Option<(usize, usize)> {
        let needle = "URI=\"";
        let start_val = line.find(needle)? + needle.len();
        let rest = &line[start_val..];
        let end_rel = rest.find('"')?;
        let end_val = start_val + end_rel;
        Some((start_val, end_val))
    }

    async fn mirror_binary(&mut self, url: Url) -> Result<()> {
        if !self.visited.insert(url.clone()) {
            return Ok(());
        }

        let local_path = self.path_for_url(&url, false);
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }

        println!("[BIN ] {} -> {}", url, local_path.display());

        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {}", url))?
            .error_for_status()
            .with_context(|| format!("status error for {}", url))?;

        let bytes = resp.bytes().await?;
        let mut file = tokio::fs::File::create(&local_path).await?;
        file.write_all(&bytes).await?;
        Ok(())
    }

    #[async_recursion]
    async fn mirror_manifest(&mut self, url: Url) -> Result<()> {
        if !self.visited.insert(url.clone()) {
            return Ok(());
        }

        let local_path = self.path_for_url(&url, true);
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }

        println!("[M3U8] {} -> {}", url, local_path.display());

        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {}", url))?
            .error_for_status()
            .with_context(|| format!("status error for {}", url))?;

        let text = resp.text().await?;

        // Quick check that it's an HLS manifest.
        if !text.trim_start().starts_with("#EXTM3U") {
            // Not a manifest; treat as binary to be safe.
            println!("  -> not an HLS manifest, saving as binary");
            return self.mirror_binary(url).await;
        }

        let mut orig_path = local_path.clone();
        if let Some(file_name) = orig_path.file_name().and_then(|f| f.to_str()) {
            orig_path.set_file_name(format!("{file_name}.orig"));
        } else {
            // fallback
            orig_path.set_file_name("manifest.m3u8.orig");
        }

        let mut orig_file = tokio::fs::File::create(&orig_path).await?;
        orig_file.write_all(text.as_bytes()).await?;

        let mut output_lines = Vec::new();
        let local_dir = local_path
            .parent()
            .ok_or_else(|| anyhow!("manifest path has no parent: {}", local_path.display()))?
            .to_path_buf();

        for line in text.lines() {
            let trimmed = line.trim();

            // Comment / tag lines
            if trimmed.starts_with('#') {
                // Handle tags with URI attributes (KEY, MEDIA, I-FRAME-STREAM-INF, etc.).
                if let Some((start, end)) = Self::find_uri_attr(line) {
                    let uri_val = &line[start..end];
                    let child_url = url.join(uri_val).with_context(|| {
                        format!("resolving URI '{}' relative to {}", uri_val, url)
                    })?;

                    let is_manifest = child_url.path().to_ascii_lowercase().ends_with(".m3u8");

                    if is_manifest {
                        self.mirror_manifest(child_url.clone()).await?;
                    } else {
                        self.mirror_binary(child_url.clone()).await?;
                    }

                    let target_path = self.path_for_url(&child_url, is_manifest);
                    let rel = Self::to_posix_relative(&target_path, &local_dir);

                    let mut new_line = String::new();
                    new_line.push_str(&line[..start]);
                    new_line.push_str(&rel);
                    new_line.push_str(&line[end..]);
                    output_lines.push(new_line);
                } else {
                    output_lines.push(line.to_string());
                }
                continue;
            }

            // Blank line
            if trimmed.is_empty() {
                output_lines.push(line.to_string());
                continue;
            }

            // Non-comment, non-empty line in HLS is a URI.
            let uri_val = trimmed;
            let child_url = url
                .join(uri_val)
                .with_context(|| format!("resolving URI '{}' relative to {}", uri_val, url))?;

            let is_manifest = child_url.path().to_ascii_lowercase().ends_with(".m3u8");

            if is_manifest {
                self.mirror_manifest(child_url.clone()).await?;
            } else {
                self.mirror_binary(child_url.clone()).await?;
            }

            let target_path = self.path_for_url(&child_url, is_manifest);
            let rel = Self::to_posix_relative(&target_path, &local_dir);
            output_lines.push(rel);
        }

        // Rewritten manifest (this is the one you actually serve)
        let mut file = tokio::fs::File::create(&local_path).await?;
        file.write_all(output_lines.join("\n").as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let start_url = Url::parse(&args.start_url)
        .with_context(|| format!("parsing start URL '{}'", args.start_url))?;

    let out_dir = args.output_dir;
    tokio::fs::create_dir_all(&out_dir)
        .await
        .with_context(|| format!("creating output dir {}", out_dir.display()))?;

    let master_url = start_url.clone();
    let master_components = master_url
        .path()
        .trim_start_matches('/')
        .split('/')
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    let mut mirror = Mirror::new(out_dir, master_components);
    mirror.mirror_manifest(start_url).await?;

    println!("Done.");
    Ok(())
}
