-- VLC-only shortcuts. The remote owns stop acknowledgement and episode choice.
-- Usage: require("spela").start("http://your-spela-host:7890")
local M = {}
local bindings, watcher = {}, nil
function M.stop()
    if watcher then watcher:stop(); watcher = nil end
    for _, key in ipairs(bindings) do key:delete() end
    bindings = {}
end
function M.start(base)
    assert(type(base) == "string" and base:match("^https?://[%w%.:%-]+$"), "Use an HTTP(S) origin without a path")
    M.stop()
    local function open(action)
        local url = base .. "/remote#/now?action=" .. action .. "&at=" .. math.floor(hs.timer.secondsSinceEpoch() * 1000)
        -- Reuse the remote's existing Chrome tab instead of opening one per keypress.
        -- Origin is validated above and action is a constant, so AppleScript literals
        -- contain no untrusted quotes or executable fragments.
        local script = [[
          tell application "Google Chrome"
            repeat with w in windows
              repeat with i from 1 to count of tabs of w
                set t to tab i of w
                if URL of t starts with "]] .. base .. [[/remote" then
                  set URL of t to "]] .. url .. [["
                  set active tab index of w to i
                  set index of w to 1
                  activate
                  return
                end if
              end repeat
            end repeat
            open location "]] .. url .. [["
            activate
          end tell]]
        local ok = hs.osascript.applescript(script)
        if not ok then hs.urlevent.openURLWithBundle(url, "com.google.Chrome") end
    end
    bindings = {
        hs.hotkey.new({"ctrl", "alt"}, "n", function() open("next") end),
        hs.hotkey.new({"ctrl", "alt"}, "h", function() open("home") end),
    }
    local function update()
        local app = hs.application.frontmostApplication()
        local enabled = app and app:bundleID() == "org.videolan.vlc"
        for _, key in ipairs(bindings) do
            if enabled then key:enable() else key:disable() end
        end
    end
    watcher = hs.application.watcher.new(update):start()
    update()
    return M
end
return M
