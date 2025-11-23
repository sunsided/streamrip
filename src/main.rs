use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use async_recursion::async_recursion;
use clap::Parser;
use pathdiff::diff_paths;
use reqwest::Client;
use roxmltree::{Document, Node};
use tokio::io::AsyncWriteExt;
use url::Url;

#[derive(Parser, Debug)]
#[command(about = "Recursively mirror an HLS (.m3u8) or DASH (.mpd) stream for local hosting")]
struct Args {
    /// Starting manifest URL (master .m3u8 or .mpd)
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
            .user_agent("stream-mirror/0.1")
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
    ///
    /// Uses the *master manifest’s URL path* as the base and preserves only the
    /// relative suffix under the output directory.
    fn path_for_url(&mut self, url: &Url, is_manifest: bool) -> PathBuf {
        if let Some(existing) = self.url_to_path.get(url) {
            return existing.clone();
        }

        let rel = url
            .path()
            .trim_start_matches('/')
            .split('/')
            .collect::<Vec<_>>();

        let base = self.master_url_path_components.as_slice();

        // Find the relative difference:
        // master = ["x","y","z","manifest.m3u8"]
        // child  = ["x","y","z","sub","foo.m3u8"]
        // -> rel_parts = ["sub","foo.m3u8"]
        let mut idx = 0;
        while idx < base.len().saturating_sub(1)
            && idx < rel.len().saturating_sub(1)
            && base[idx] == rel[idx]
        {
            idx += 1;
        }

        let rel_parts = rel[idx..].join("/");

        // Local path under output dir:
        let mut local_path = self.out_dir.join(rel_parts);

        // Ensure manifest has a .m3u8 extension if it has none
        if is_manifest && local_path.extension().is_none() {
            local_path.set_extension("m3u8");
        }

        // Handle query string → safe filenames
        if let Some(q) = url.query() {
            let fname = local_path.file_name().unwrap_or_default().to_string_lossy();
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

    /// Mirror an HLS manifest (.m3u8), rewriting all URIs to local relative paths.
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
            println!("  -> not an HLS manifest, saving as binary");
            return self.mirror_binary(url).await;
        }

        // Save original manifest next to rewritten one
        let mut orig_path = local_path.clone();
        if let Some(file_name) = orig_path.file_name().and_then(|f| f.to_str()) {
            orig_path.set_file_name(format!("{file_name}.orig"));
        } else {
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

    /// Mirror a DASH MPD: save MPD as-is, but download all referenced segments / sidecars.
    async fn mirror_mpd(&mut self, url: Url) -> Result<()> {
        if !self.visited.insert(url.clone()) {
            return Ok(());
        }

        let local_path = self.path_for_url(&url, true);
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }

        println!("[MPD ] {} -> {}", url, local_path.display());

        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {}", url))?
            .error_for_status()
            .with_context(|| format!("status error for {}", url))?;

        let text = resp.text().await?;

        // Save original
        let mut orig_path = local_path.clone();
        if let Some(file_name) = orig_path.file_name().and_then(|f| f.to_str()) {
            orig_path.set_file_name(format!("{file_name}.orig"));
        } else {
            orig_path.set_file_name("manifest.mpd.orig");
        }
        let mut orig_file = tokio::fs::File::create(&orig_path).await?;
        orig_file.write_all(text.as_bytes()).await?;

        // Save "rewritten" (we keep content identical for now)
        let mut file = tokio::fs::File::create(&local_path).await?;
        file.write_all(text.as_bytes()).await?;

        // Parse MPD and discover segments
        let doc = Document::parse(&text)?;
        let root = doc.root_element();
        if root.tag_name().name() != "MPD" {
            println!("  -> not an MPD root element, treating as binary");
            return self.mirror_binary(url).await;
        }

        let mpd_duration_secs = root
            .attribute("mediaPresentationDuration")
            .and_then(parse_iso8601_duration_seconds);

        let mpd_url = url.clone();

        // Walk: MPD -> Period -> AdaptationSet -> Representation
        for period in root
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "Period")
        {
            // Period BaseURL (e.g. "dash/")
            let period_base = if let Some(b) = first_child_text(&period, "BaseURL") {
                mpd_url
                    .join(b.trim())
                    .with_context(|| format!("joining Period BaseURL '{}' to {}", b, mpd_url))?
            } else {
                mpd_url.clone()
            };

            for aset in period
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "AdaptationSet")
            {
                // AdaptationSet BaseURL overrides Period BaseURL if present
                let aset_base = if let Some(b) = first_child_text(&aset, "BaseURL") {
                    period_base.join(b.trim()).with_context(|| {
                        format!("joining AdaptationSet BaseURL '{}' to {}", b, period_base)
                    })?
                } else {
                    period_base.clone()
                };

                // Optional SegmentTemplate at AdaptationSet level
                let aset_st = first_child_element(&aset, "SegmentTemplate");

                for rep in aset
                    .children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "Representation")
                {
                    let rep_id = match rep.attribute("id") {
                        Some(id) => id.to_string(),
                        None => continue,
                    };

                    // Representation BaseURL overrides AdaptationSet BaseURL if present
                    let (rep_base, rep_base_is_file) =
                        if let Some(b) = first_child_text(&rep, "BaseURL") {
                            let is_file = !b.trim_end().ends_with('/');
                            let url = aset_base.join(b.trim()).with_context(|| {
                                format!("joining Representation BaseURL '{}' to {}", b, aset_base)
                            })?;
                            (url, is_file)
                        } else {
                            (aset_base.clone(), false)
                        };

                    // Representation-level SegmentTemplate or fallback to AdaptationSet-level
                    let rep_st = first_child_element(&rep, "SegmentTemplate").or(aset_st);

                    if let Some(st) = rep_st {
                        // Handle SegmentTemplate-based segments
                        self.handle_segment_template(
                            &mpd_url,
                            &rep_base,
                            &rep_id,
                            st,
                            mpd_duration_secs,
                        )
                        .await?;
                    }

                    // If there was a Representation BaseURL that looks like a file
                    // (e.g. "textstream_eng=1000.webvtt"), download it.
                    if rep_base_is_file {
                        self.mirror_binary(rep_base.clone()).await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle a <SegmentTemplate> for a given Representation.
    async fn handle_segment_template(
        &mut self,
        _mpd_url: &Url,
        base_url: &Url,
        representation_id: &str,
        st: Node<'_, '_>,
        mpd_duration_secs: Option<f64>,
    ) -> Result<()> {
        let init_tmpl = st.attribute("initialization");
        if let Some(tmpl) = init_tmpl {
            let path = tmpl.replace("$RepresentationID$", representation_id);
            let full = base_url
                .join(path.trim())
                .with_context(|| format!("joining init path '{}' to {}", path, base_url))?;
            self.mirror_binary(full).await?;
        }

        let media_tmpl = match st.attribute("media") {
            Some(v) if !v.is_empty() => v,
            _ => return Ok(()),
        };

        let timescale = st
            .attribute("timescale")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1);
        let duration_units = st.attribute("duration").and_then(|v| v.parse::<u64>().ok());
        let start_number = st
            .attribute("startNumber")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1);
        let end_number_attr = st
            .attribute("endNumber")
            .and_then(|v| v.parse::<u64>().ok());

        let end_number = if let Some(en) = end_number_attr {
            en
        } else if let (Some(dur_u), Some(total_secs)) = (duration_units, mpd_duration_secs) {
            let seg_secs = dur_u as f64 / timescale as f64;
            let count = (total_secs / seg_secs).ceil() as u64;
            start_number + count - 1
        } else {
            println!(
                "  -> Skipping media segments for {} (no endNumber and no duration/MPD duration)",
                representation_id
            );
            return Ok(());
        };

        for num in start_number..=end_number {
            let mut path = media_tmpl.replace("$RepresentationID$", representation_id);
            path = path.replace("$Number$", &num.to_string());
            let full = base_url
                .join(path.trim())
                .with_context(|| format!("joining media path '{}' to {}", path, base_url))?;
            self.mirror_binary(full).await?;
        }

        Ok(())
    }
}

/// Get the text of the first child element with given name, if any.
fn first_child_text<'a>(node: &Node<'a, 'a>, name: &str) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(|s| s.to_string())
}

/// Get the first child element with given name, if any.
fn first_child_element<'a>(node: &Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

/// Parse a minimal ISO 8601 duration like "PT3M30.840S" into seconds.
fn parse_iso8601_duration_seconds(s: &str) -> Option<f64> {
    if !s.starts_with("PT") {
        return None;
    }
    let mut rest = &s[2..];
    let mut hours = 0.0;
    let mut mins = 0.0;
    let mut secs = 0.0;

    while !rest.is_empty() {
        let mut i = 0;
        let bytes = rest.as_bytes();
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c.is_ascii_digit() || c == '.' {
                i += 1;
            } else {
                break;
            }
        }
        if i == 0 || i >= rest.len() {
            break;
        }
        let (num_str, tail) = rest.split_at(i);
        let val: f64 = num_str.parse().ok()?;
        let unit = tail.chars().next()?;
        rest = &tail[1..];

        match unit {
            'H' => hours = val,
            'M' => mins = val,
            'S' => secs = val,
            _ => return None,
        }
    }

    Some(hours * 3600.0 + mins * 60.0 + secs)
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

    let master_components = start_url
        .path()
        .trim_start_matches('/')
        .split('/')
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    let mut mirror = Mirror::new(out_dir, master_components);

    let ext = start_url
        .path()
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "m3u8" => mirror.mirror_manifest(start_url).await?,
        "mpd" => mirror.mirror_mpd(start_url).await?,
        other => return Err(anyhow!("Unsupported start URL extension: {}", other)),
    }

    println!("Done.");
    Ok(())
}
