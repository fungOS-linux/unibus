use mlua::Lua;
fn run(test: &str) {
    let lua = Lua::new();
    let v1: mlua::Table = lua
        .load(include_str!("../lua/agent_sessions.lua"))
        .eval()
        .unwrap();
    let v2: mlua::Table = lua
        .load(include_str!("../lua/agent_sessions_v2.lua"))
        .eval()
        .unwrap();
    lua.globals().set("v1", v1).unwrap();
    lua.globals().set("v2", v2).unwrap();
    lua.load("agents=v2.new(v1)").exec().unwrap();
    lua.load(test).exec().unwrap();
}
#[test]
fn explicit_waits_and_privacy() {
    run(r#"
        assert(v2.attention('codex',{status={type='active',activeFlags={'waitingOnUserInput'}}})=='input')
        assert(v2.attention('codex',{status={type='active',activeFlags={'waitingOnApproval'}}})=='approval')
        assert(v2.attention('codex',{status={type='active',activeFlags={'newFlag'}}})==nil)
        assert(v2.attention('claude-code',{hook_event_name='Notification',notification_type='idle_prompt'})==nil)
        assert(v2.attention('claude-code',{hook_event_name='Notification',notification_type='permission_prompt'})=='approval')
        assert(v2.attention('operator',{pending_approval=true})=='approval')
        assert(v2.attention('opencode',{status={type='retry'}})==nil)
        local input={provider='codex',host_id='neo',instance_id='a',sequence=1,mode='snapshot',sessions={
          {id='a',status={type='active',activeFlags={'waitingOnApproval'}},display_project='Canvas',display_task='TV panel',prompt='secret',cwd='/private/path'}}}
        local out=agents.normalize(input,1000)
        assert(out.schema_version==2 and out.sessions[1].attention.reason=='approval')
        assert(out.sessions[1].project=='Canvas' and out.sessions[1].task=='TV panel')
        assert(out.sessions[1].prompt==nil and out.sessions[1].cwd==nil)
        assert(v1.normalize(input,1000).sessions[1].project==nil)
        input.sequence=2;input.sessions[1].display_task='bad\nlabel'
        assert(not pcall(agents.normalize,input,2000))
    "#);
}
#[test]
fn waiting_timer_resets_on_clear_gap_restart_and_rejects_replay() {
    run(r#"
        local input={provider='operator',host_id='neo',instance_id='a',sequence=1,mode='snapshot',sessions={{id='a',pending_approval=true}}}
        assert(agents.normalize(input,1000).sessions[1].attention.since_unix_ms==1000)
        assert(not pcall(agents.normalize,input,2000))
        input.sequence=2
        assert(agents.normalize(input,2000).sessions[1].attention.since_unix_ms==1000)
        input.sequence=3;input.sessions[1].pending_approval=false
        assert(agents.normalize(input,3000).sessions[1].attention==nil)
        input.sequence=4;input.sessions[1].pending_approval=true
        assert(agents.normalize(input,4000).sessions[1].attention.since_unix_ms==4000)
        input.sequence=5
        assert(agents.normalize(input,40000).sessions[1].attention.since_unix_ms==40000)
        input.instance_id='b';input.sequence=1
        assert(agents.normalize(input,41000).sessions[1].attention.since_unix_ms==41000)
    "#);
}

#[test]
fn scopes_and_retained_events_are_bounded() {
    run(r#"
      local input={provider='operator',host_id='neo',instance_id='a',sequence=1,mode='snapshot',sessions={}}
      for n=1,64 do input.instance_id=tostring(n);agents.normalize(input,1000) end
      input.instance_id='overflow';assert(not pcall(agents.normalize,input,1000))
      agents=v2.new(v1);input.instance_id='one'
      for n=1,512 do input.sessions[n]={id=tostring(n),pending_approval=true} end
      agents.normalize(input,1000)
      input.mode='event';input.sequence=2;input.sessions={{id='513',pending_approval=true}}
      assert(not pcall(agents.normalize,input,2000))
      input.sessions={{id='1',state='ended'}};agents.normalize(input,2000)
      input.sequence=3;input.sessions={{id='513',pending_approval=true}}
      assert(agents.normalize(input,3000).sessions[1].attention.reason=='approval')
    "#);
}
