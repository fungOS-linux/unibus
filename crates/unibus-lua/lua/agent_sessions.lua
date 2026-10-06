-- Provider-neutral telemetry. The host owns IO, identity, authorization and clock.
-- No prompts, terminal text, tokens, titles or working-directory paths cross the bus.
local M = {}
local function nonempty(value)
    return type(value) == "string" and #value > 0 and #value <= 256
end

function M.state(provider, item)
    if provider == "codex" then
        local status = item.status or {}
        if status.type == "idle" then return "idle" end
        if status.type == "active" then
            -- Waiting flags do not mean computation is active.
            if type(status.activeFlags) ~= "table" then return "unknown" end
            if next(status.activeFlags) ~= nil then return "unknown" end
            return "active"
        end
    elseif provider == "opencode" then
        local status = item.status or item
        if status.type == "busy" then return "active" end
        if status.type == "idle" then return "idle" end
        -- retry is neither working nor idle.
    elseif provider == "claude-code" then
        local event = item.hook_event_name
        if event == "UserPromptSubmit" or event == "PreToolUse"
            or event == "PostToolUse" or event == "PostToolUseFailure" then
            return "active"
        end
        if event == "Stop" or event == "TeammateIdle" then
            return "idle"
        end
        if event == "SessionEnd" then return "ended" end
        if event == "Notification" and item.notification_type == "idle_prompt" then
            return "idle"
        end
    elseif provider == "operator" then
        -- Existing owner roster vocabulary; do not infer from process presence.
        if item.pending_approval == true then return "unknown" end
        local state = item.state or item.status
        if state == "working" or state == "running" or state == "active"
            or state == "prompting" then return "active" end
        if state == "idle" then return "idle" end
        if state == "ended" or state == "completed" or state == "failed"
            or state == "cancelled" or state == "terminated" then return "ended" end
    else
        -- Other owners may explicitly implement the normalized vocabulary.
        if item.state == "active" or item.state == "idle" or item.state == "ended" then
            return item.state
        end
    end
    return "unknown"
end

-- A complete source snapshot replaces only this owner scope. Hook events merge
-- one session; they must never erase other sessions from the same Claude host.
function M.normalize(input, now_ms)
    assert(type(input) == "table", "expected owner observation")
    assert(nonempty(input.provider) and nonempty(input.host_id)
        and nonempty(input.instance_id), "missing owner scope")
    assert(type(now_ms) == "number" and now_ms >= 0 and now_ms % 1 == 0,
        "invalid host clock")
    assert(input.mode == "snapshot" or input.mode == "event", "invalid update mode")
    assert(type(input.sessions) == "table" and #input.sessions <= 512, "invalid session list")
    assert(type(input.sequence) == "number" and input.sequence >= 0
        and input.sequence % 1 == 0, "invalid owner sequence")
    local sessions, seen = {}, {}
    for _, item in ipairs(input.sessions) do
        local id = item.session_id or item.id
        assert(nonempty(id), "missing session id")
        local state = M.state(input.provider, item)
        if not seen[id] then
            seen[id] = state
            sessions[#sessions + 1] = {id = id, state = state}
        else
            assert(seen[id] == state, "conflicting duplicate session")
        end
    end
    table.sort(sessions, function(a, b) return a.id < b.id end)
    return {
        schema_version = 1, provider = input.provider, host_id = input.host_id,
        instance_id = input.instance_id, mode = input.mode,
        sequence = input.sequence, observed_at_unix_ms = now_ms,
        sessions = sessions,
    }
end

function M.publish(bus, input, now_ms)
    return bus:publish({kind = "agent.sessions.v1", body = M.normalize(input, now_ms)})
end

return M
