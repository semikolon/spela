-- VLC-only shortcuts. The remote owns stop acknowledgement and episode choice.
-- Usage: require("spela").start("http://your-spela-host:7890")
local M = {}
local bindings, watcher, timer = {}, nil, nil
local generation = 0
function M.stop()
    generation = generation + 1
    if timer then timer:stop(); timer = nil end
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
        hs.hotkey.new({}, "n", function() open("next") end),
        hs.hotkey.new({}, "h", function() open("home") end),
        hs.hotkey.new({"ctrl", "alt"}, "n", function() open("next") end),
        hs.hotkey.new({"ctrl", "alt"}, "h", function() open("home") end),
    }
    local nearEnd, sampledAt, trackedTitle = false, 0, nil
    local function focused()
        local app = hs.application.frontmostApplication()
        return app and app:bundleID() == "org.videolan.vlc"
    end
    local function update()
        local active = focused()
        local fresh = hs.timer.secondsSinceEpoch() - sampledAt < 6
        for i, key in ipairs(bindings) do
            -- Modified shortcuts remain available throughout VLC playback.
            if active and (i > 2 or (nearEnd and fresh)) then key:enable() else key:disable() end
        end
    end
    local function decode(status, body)
        if status ~= 200 then return nil end
        local ok, value = pcall(hs.json.decode, body)
        if ok and type(value) == "table" then return value end
    end
    local function poll()
        generation = generation + 1
        local request = generation
        update() -- Expire eligibility even if an HTTP request never answers.
        if not focused() then nearEnd=false; trackedTitle=nil; return end
        hs.http.asyncGet(base .. "/status", nil, function(status, body)
            if request ~= generation then return end
            local state = decode(status, body)
            local title = state and state.vlc_stream_title
            if type(title) ~= "string" or title == "" then nearEnd=false; update(); return end
            if title ~= trackedTitle then nearEnd=false; trackedTitle=title; update() end
            hs.http.asyncGet(base .. "/api/position?title=" .. hs.http.encodeForQuery(title), nil, function(code, data)
                if request ~= generation then return end
                local position = decode(code, data)
                local t = position and tonumber(position.t)
                local duration = position and tonumber(position.dur)
                -- Same reported-position threshold as the remote's Up next panel.
                nearEnd = not not (position and not position.gone and t and duration
                    and duration >= 300 and t >= duration - math.min(120, duration * .04))
                sampledAt = hs.timer.secondsSinceEpoch()
                update()
            end)
        end)
    end
    watcher = hs.application.watcher.new(function()
        nearEnd=false; update(); poll()
    end):start()
    timer = hs.timer.doEvery(2, poll)
    poll()
    return M
end
return M
