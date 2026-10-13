-- Offline harness: no VLC, Chrome, or network traffic.
local keys, requests, script, focusChanged, tick = {}, {}, nil, nil, nil
local front, now, timerStopped = "org.videolan.vlc", 12345, false
hs = {
    hotkey = { new = function(mods, key, fn)
        local k = {key=key, mods=mods, press=fn, enabled=false}
        function k:enable() self.enabled=true end
        function k:disable() self.enabled=false end
        function k:delete() self.deleted=true end
        keys[#keys+1]=k
        return k
    end },
    application = {
        frontmostApplication=function() return {bundleID=function() return front end} end,
        watcher={new=function(fn)
            focusChanged=fn
            return {start=function(self) return self end, stop=function() end}
        end},
    },
    timer={secondsSinceEpoch=function() return now end, doEvery=function(_,fn)
        tick=fn; return {stop=function() timerStopped=true end}
    end},
    http={asyncGet=function(url,_,fn) requests[#requests+1]={url=url,reply=fn} end,
        encodeForQuery=function(s) return s:gsub(" ","%%20") end},
    json={decode=function(data) if type(data)~="table" then error("bad JSON") end; return data end},
    osascript={applescript=function(s) script=s; return true end},
    urlevent={openURLWithBundle=function() error("unexpected fallback") end},
}
local function reply(data,code)
    local request=table.remove(requests,1)
    assert(request,"expected request"); request.reply(code or 200,data)
end
local function sample(t,dur,gone)
    reply({vlc_stream_title="Example S01E01"})
    assert(requests[1].url:find("title=Example%20S01E01",1,true))
    reply({t=t,dur=dur,gone=gone})
end
local function plain(expected)
    assert(keys[1].enabled==expected and keys[2].enabled==expected)
end
local module=dofile("integrations/hammerspoon/spela.lua")
module.start("http://example.test:7890")
assert(#keys==4 and #keys[1].mods==0 and #keys[2].mods==0)
assert(keys[3].enabled and keys[4].enabled)
plain(false) -- Default to VLC's own keys until position is known.
sample(1200,3600); plain(false)
tick(); sample(3480,3600); plain(true)
keys[1].press(); assert(script:find("action=next&at=12345000",1,true))
keys[2].press(); assert(script:find("action=home&at=12345000",1,true))
tick(); sample(3400,3600); plain(false) -- Seeking back restores VLC's keys.
tick(); sample(1152,1200); plain(true) -- Shorter episode: final 4%, not 2 minutes.
tick(); sample(1151,1200); plain(false)
tick(); sample(3590,3600,true); plain(false) -- Closed player.
tick(); sample(0,0); plain(false) -- Unknown duration.
tick(); sample(3480,3600); plain(true)
tick(); reply("unavailable",503); plain(false)
tick(); sample(3480,3600); plain(true)
tick(); now=now+7; tick(); plain(false) -- Hung HTTP must expire eligibility.
requests={} -- Discard timed-out requests in this offline harness.
tick(); reply({vlc_stream_title="Example S01E01"})
local stale=table.remove(requests,1)
front="com.google.Chrome"; focusChanged()
for _,key in ipairs(keys) do assert(not key.enabled) end
stale.reply(200,{t=3590,dur=3600})
plain(false) -- Late reply cannot re-enable keys after focus changes.
front="org.videolan.vlc"; focusChanged(); plain(false)
sample(3480,3600); plain(true)
tick(); reply({vlc_stream_title="Example S01E02"}); plain(false)
reply({t=10,dur=3600}); plain(false) -- New episode drops old eligibility.
tick(); reply({vlc_stream_title="Example S01E02"})
local late=table.remove(requests,1)
module.stop(); late.reply(200,{t=3590,dur=3600})
for _,key in ipairs(keys) do assert(key.deleted) end
assert(timerStopped)
assert(not pcall(module.start, 'http://example.test/"bad'))
print("Hammerspoon handoff: all pass")
