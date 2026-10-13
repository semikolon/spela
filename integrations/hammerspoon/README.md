# VLC shortcuts

Copy `spela.lua` into `~/.hammerspoon/`, then add this to `init.lua`, using your server's origin:

```lua
require("spela").start("http://your-spela-host:7890")
```

Reload Hammerspoon. In VLC, **N** plays the next aired episode and **H** stops playback and opens Home. **Ctrl+Alt+N/H** remain aliases. These bindings override VLC’s own actions for N and H only while VLC is the frontmost app; arrow keys remain untouched. In the web remote, use **Ctrl+Alt+N/H**, so ordinary typing cannot trigger playback actions.

The shortcut reuses a Chrome remote tab (or opens one), with an expiring, one-shot action. The remote waits for the VLC bridge to confirm that the old player has closed before launching the next episode. It shows a persistent error if the bridge cannot confirm the stop. Hammerspoon may ask for macOS permission to control Chrome on first use.

The large Up next panel is in the web remote. It does not overlay fullscreen VLC or automatically skip credits. Metadata is prepared during playback; media downloads begin on an explicit play action. The existing `spela-vlc-bridge` must be running for VLC control.
