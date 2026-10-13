# VLC shortcuts

Copy `spela.lua` into `~/.hammerspoon/`, then add this to `init.lua`, using your server's origin:

```lua
require("spela").start("http://your-spela-host:7890")
```

Reload Hammerspoon. **Ctrl+Alt+N** plays the next aired episode; **Ctrl+Alt+H** stops playback and opens Home. The bindings are enabled only while VLC is the frontmost app. The same keys work directly in the web remote.

The shortcut reuses a Chrome remote tab (or opens one), with an expiring, one-shot action. The remote waits for the VLC bridge to confirm that the old player has closed before launching the next episode. It shows a persistent error if the bridge cannot confirm the stop. Hammerspoon may ask for macOS permission to control Chrome on first use.

The large Up next panel is in the web remote. It does not overlay fullscreen VLC or automatically skip credits. Metadata is prepared during playback; media downloads begin on an explicit play action. The existing `spela-vlc-bridge` must be running for VLC control.
