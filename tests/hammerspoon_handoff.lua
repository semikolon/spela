-- Offline harness: verifies bindings without launching VLC or Chrome.
local keys, script, callback = {}, nil, nil
local front = "org.videolan.vlc"
hs = {
    hotkey = { new = function(mods, key, fn)
        local k = {key=key, press=fn, enabled=false}
        function k:enable() self.enabled=true end
        function k:disable() self.enabled=false end
        function k:delete() self.deleted=true end
        keys[#keys+1]=k
        return k
    end },
    application = {
        frontmostApplication=function() return {bundleID=function() return front end} end,
        watcher={new=function(fn)
            callback=fn
            return {start=function(self) return self end, stop=function() end}
        end},
    },
    timer={secondsSinceEpoch=function() return 12345 end},
    osascript={applescript=function(s) script=s; return true end},
    urlevent={openURLWithBundle=function() error("unexpected fallback") end},
}
local module=dofile("integrations/hammerspoon/spela.lua")
module.start("http://example.test:7890")
assert(#keys==2 and keys[1].enabled and keys[2].enabled)
keys[1].press()
assert(script:find("action=next&at=12345000",1,true))
keys[2].press()
assert(script:find("action=home&at=12345000",1,true))
front="com.google.Chrome"; callback()
assert(not keys[1].enabled and not keys[2].enabled)
module.stop()
assert(keys[1].deleted and keys[2].deleted)
assert(not pcall(module.start, 'http://example.test/"bad'))
print("Hammerspoon handoff: all pass")
