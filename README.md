# StreamRip

A command-line tool that recursively mirrors DASH ( Dynamic Adaptive Streaming over HTT) or HLS (HTTP Live Streaming)
content by downloading manifests and media segments while rewriting manifest URLs for local hosting.

## Features

- Downloads complete DASH or HLS streams including master playlists, media playlists, segments and text tracks
- Maintains the relative path structure from the source
- Rewrites manifest URLs to work with local hosting
- Handles query parameters in URLs by converting them to safe filenames
- Preserves original manifests with `.orig` extension for reference

## Example Usage

```shell
streamrip --start-url=https://example.com/stream/97333-f40e7a11-73a2-47df-a767-9f0bcdfb83cd.ism/manifest.m3u8 --output-dir=hls
streamrip --start-url=https://example.com/stream/97333-f40e7a11-73a2-47df-a767-9f0bcdfb83cd.ism/manifest.mpd  --output-dir=dash
```
