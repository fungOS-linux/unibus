-- Opt-in attention metadata. V1 remains private and unchanged.
-- The trusted host owns IO and supplies the clock; no terminal/prompt parsing.
local M = {}
local function label(value)
    if value == nil then return nil end
    assert(type(value) == "string" and #value <= 160 and not value:find("[%c]"),
        "invalid display label")
    return value ~= "" and value or nil
end
function M.attention(provider, item)
    if provider == "codex" and type(item.status) == "table"
        and item.status.type == "active" and type(item.status.activeFlags) == "table" then
        local input, approval = false, false
        for _, flag in ipairs(item.status.activeFlags) do
            if flag == "waitingOnApproval" then approval = true
            elseif flag == "waitingOnUserInput" then input = true end
        end
        if approval then return "approval" end
        if input then return "input" end
    elseif provider == "claude-code" and item.hook_event_name == "Notification" then
        if item.notification_type == "permission_prompt" then return "approval" end
        -- idle_prompt is not proof of a pending question.
    elseif provider == "operator" and item.pending_approval == true then
        return "approval"
    end
    if item.attention == "input" or item.attention == "approval" then
        return item.attention
    end
end
function M.new(v1)
    local owners = {}
    local api = {}
    function api.normalize(input, now_ms)
        local out = v1.normalize(input, now_ms)
        assert(not input.host_id:find("[%c]") and not input.provider:find("[%c]")
            and not input.instance_id:find("[%c]"), "invalid owner scope")
        local key = input.host_id .. "\0" .. input.provider .. "\0" .. input.instance_id
        local owner = owners[key]
        if owner then assert(input.sequence > owner.sequence, "replayed owner sequence")
            assert(now_ms >= owner.observed, "owner clock went backwards")
        else
            local count = 0
            for _ in pairs(owners) do count = count + 1 end
            assert(count < 64, "owner budget exceeded")
            owner = {sessions = {}}
        end
        local source = {}
        for _, item in ipairs(input.sessions) do
            local id = item.session_id or item.id
            assert(not id:find("[%c]"), "invalid session identity")
            local detail = {project = label(item.display_project), task = label(item.display_task),
                reason = M.attention(input.provider, item)}
            if source[id] then
                local old = source[id]
                assert(old.project == detail.project and old.task == detail.task
                    and old.reason == detail.reason, "conflicting duplicate details")
            end
            source[id] = detail
        end
        local retained = {}
        if input.mode == "event" then for id, entry in pairs(owner.sessions) do retained[id] = entry end end
        for _, session in ipairs(out.sessions) do
            local detail = source[session.id]
            session.project, session.task = detail.project, detail.task
            if detail.reason and session.state ~= "ended" then
                local previous = owner.sessions[session.id]
                local since = now_ms
                if previous and previous.reason == detail.reason
                    and now_ms - previous.observed <= 30000 then since = previous.since end
                session.attention = {reason = detail.reason, since_unix_ms = since,
                    timing = "first_observed"}
                retained[session.id] = {reason = detail.reason, since = since, observed = now_ms}
            else retained[session.id] = nil end
        end
        local count=0;for _ in pairs(retained) do count=count+1 end
        assert(count<=512,"retained attention budget exceeded")
        owners[key] = {sessions = retained, sequence = input.sequence, observed = now_ms}
        out.schema_version = 2
        return out
    end
    return api
end
return M
