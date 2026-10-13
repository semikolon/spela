# VLC shortcuts

Copy `spela.lua` into `~/.hammerspoon/`, then add this to `init.lua`, using your server's origin:

```lua
require("spela").start("http://your-spela-host:7890")
```

Reload Hammerspoon. Near the end of playback in VLC, **N** plays the next aired episode and **H** stops playback and opens Home. Plain N/H override VLC’s own actions only while VLC is frontmost and Spela reports the final 4% of runtime, capped at two minutes (for media at least five minutes long, matching the Up next panel). Earlier in playback, or with unknown/unavailable playback status, VLC keeps its normal N/H actions. **Ctrl+Alt+N/H** remain available throughout while VLC is frontmost; arrow keys remain untouched.

The integration checks Spela every two seconds while VLC is frontmost, using the bridge’s reported playhead, so seek changes take effect after the next bridge report. Failed or stale HTTP responses disable the plain-key overrides. It does not infer credits from the picture. In the web remote, plain **N/H** work while the end-of-playback offer is visible, including after playback ends. They are disabled in text fields and when the offer is dismissed. **Ctrl+Alt+N/H** remain available outside that end window.

The shortcut reuses a Chrome remote tab (or opens one), with an expiring, one-shot action. The remote waits for the VLC bridge to confirm that the old player has closed before launching the next episode. It shows a persistent error if the bridge cannot confirm the stop. Hammerspoon may ask for macOS permission to control Chrome on first use.

The large Up next panel is in the web remote. It does not overlay fullscreen VLC or automatically skip credits. Metadata is prepared during playback; media downloads begin on an explicit play action. The existing `spela-vlc-bridge` must be running for VLC control.
