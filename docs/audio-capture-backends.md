# Application audio capture

`unibus-audio-tap` selects a GStreamer capture provider using `backend` and
`capture_device`. Existing configurations default to `alsa`.

| backend | capture_device |
| --- | --- |
| alsa | PCM name, e.g. `visualizer_capture` |
| jack | Exact comma-separated source ports, e.g. `surge:left,surge:right` |
| pipewire | PipeWire source node path/ID |
| coreaudio | Numeric CoreAudio input device ID (virtual devices supported) |

The corresponding GStreamer plugin must be installed. Missing plugins and
capture failures surface on stderr and terminate the tap. JACK uses explicit
port connections and requires GStreamer 1.20 or newer. Capture is converted and
resampled to the configured PCM geometry before publication through the existing
`audio.playback_chunk` contract.

This adapter publishes bounded PCM chunks. Studio transport still requires
latency measurements; capture-provider selection alone does not establish
low-latency monitoring. Application discovery, route receipts, and playback
provider selection remain separate work.
