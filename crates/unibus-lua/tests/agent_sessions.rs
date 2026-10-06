use mlua::Lua;

fn run(test: &str) {
    let lua = Lua::new();
    let module: mlua::Table = lua
        .load(include_str!("../lua/agent_sessions.lua"))
        .eval()
        .unwrap();
    lua.globals().set("agents", module).unwrap();
    lua.load(test).exec().unwrap();
}

#[test]
fn provider_states_are_explicit() {
    run(r#"
        assert(agents.state("codex", {status={type="active",activeFlags={}}}) == "active")
        assert(agents.state("codex", {status={type="active"}}) == "unknown")
        assert(agents.state("codex", {status={type="active",activeFlags={"waitingOnApproval"}}}) == "unknown")
        assert(agents.state("opencode", {type="busy"}) == "active")
        assert(agents.state("opencode", {type="retry"}) == "unknown")
        assert(agents.state("opencode", {type="idle"}) == "idle")
        assert(agents.state("claude-code", {hook_event_name="UserPromptSubmit"}) == "active")
        assert(agents.state("claude-code", {hook_event_name="Stop"}) == "idle")
        assert(agents.state("claude-code", {hook_event_name="SessionEnd"}) == "ended")
        assert(agents.state("operator", {state="working"}) == "active")
        assert(agents.state("operator", {state="working",pending_approval=true}) == "unknown")
        assert(agents.state("operator", {state="blocked"}) == "unknown")
        assert(agents.state("other", {state="idle"}) == "idle")
        assert(agents.state("other", {state="probably_running"}) == "unknown")
    "#);
}

#[test]
fn observation_is_deduplicated_and_private() {
    run(r#"
        local input = {provider="operator", host_id="neo", instance_id="owner-1",
            mode="snapshot", sequence=1, sessions={
                {id="b",state="working",prompt="secret",title="private"},
                {id="a",state="idle"}, {id="b",state="working"}}}
        local out = agents.normalize(input, 1000)
        assert(#out.sessions == 2 and out.sessions[1].id == "a")
        assert(out.sessions[2].prompt == nil and out.sessions[2].title == nil)
        assert(out.observed_at_unix_ms == 1000 and out.mode == "snapshot")
        input.sessions[3].state="idle"
        assert(not pcall(agents.normalize,input,1000))
        input.sessions[3].state="working"
        input.mode="event"
        assert(agents.normalize(input,1000).mode == "event")
        input.host_id=""
        assert(not pcall(agents.normalize,input,1000))
    "#);
}

#[test]
fn publishes_data_not_a_widget() {
    run(r#"
        local bus = {publish=function(self,message)
            assert(message.kind == "agent.sessions.v1")
            assert(message.body.widget == nil)
            return "request-1"
        end}
        assert(agents.publish(bus,{provider="other",host_id="neo",instance_id="1",
            mode="snapshot",sequence=0,sessions={}},1000) == "request-1")
    "#);
}
