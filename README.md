# StreamRip

A command-line tool that recursively mirrors HLS (HTTP Live Streaming) content by downloading manifests and media
segments while rewriting manifest URLs for local hosting.

## Features

- Downloads complete HLS streams including master playlists, media playlists, and segments
- Maintains the relative path structure from the source
- Rewrites manifest URLs to work with local hosting
- Handles query parameters in URLs by converting them to safe filenames
- Preserves original manifests with `.orig` extension for reference
