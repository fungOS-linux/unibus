//! Generic trusted Lua adapter host: owner source -> stream -> Lua consumer.
//! It knows neither widget layout nor provider-specific session states.
use mlua::LuaSerdeExt;
use serde_json::{Value, json};
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn now_ms() -> Result<u64, Box<dyn std::error::Error>> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
fn source(args: &[Value]) -> Result<Value, Box<dyn std::error::Error>> {
    let program = args
        .first()
        .and_then(Value::as_str)
        .ok_or("missing source command")?;
    let mut command = Command::new(program);
    for arg in &args[1..] {
        command.arg(arg.as_str().ok_or("invalid source argument")?);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("missing source output")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(65537).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            let _ = reader.join();
            return Err("source timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader.join().map_err(|_| "source reader failed")??;
    if !status.success() || bytes.len() > 65536 {
        return Err("source failed or exceeded budget".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected private runtime config")?;
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let lua = mlua::Lua::new();
    unibus_lua::install(&lua)?;
    lua.globals().set("config", lua.to_value(&config)?)?;
    lua.globals().set(
        "normalizer",
        lua.load(include_str!("../lua/agent_sessions.lua"))
            .eval::<mlua::Table>()?,
    )?;
    let contract = config["contract"].as_str().unwrap_or("agent.sessions.v1");
    if contract == "agent.sessions.v2" {
        lua.globals().set(
            "attention_module",
            lua.load(include_str!("../lua/agent_sessions_v2.lua"))
                .eval::<mlua::Table>()?,
        )?;
        lua.load("normalizer=attention_module.new(normalizer)")
            .exec()?;
    } else if contract != "agent.sessions.v1" {
        return Err("unsupported agent contract".into());
    }
    lua.globals().set("agent_contract", contract)?;
    lua.globals().set(
        "adapter",
        lua.load(std::fs::read_to_string(
            config["consumer_script"]
                .as_str()
                .ok_or("missing Lua consumer")?,
        )?)
        .eval::<mlua::Table>()?,
    )?;
    lua.load(
        "publisher=unibus.connect(config.publisher); consumer=unibus.connect(config.consumer)",
    )
    .exec()?;
    lua.globals().set("event_kind", unibus::STREAM_EVENT_KIND)?;
    lua.globals()
        .set("interest_kind", unibus::STREAM_INTEREST_KIND)?;
    lua.globals().set("reply_kind", unibus::kinds::V1_REPLY)?;
    let instance = format!("{}-{}", now_ms()?, std::process::id());
    let mut sequence = 0u64;
    let mut next = Instant::now();
    loop {
        let now = now_ms()?;
        lua.globals().set("now_ms", now)?;
        if Instant::now() >= next {
            lua.load(r#"consumer:publish({kind=interest_kind,body={
                lease_id=config.consumer.producer_id .. ":agents", consumer_id=config.consumer.producer_id,
                stream_ids={config.stream_id}, issued_at_unix_ms=now_ms,expires_at_unix_ms=now_ms+30000}})"#).exec()?;
            for entry in config["sources"].as_array().ok_or("missing sources")? {
                match source(entry["command"].as_array().ok_or("missing source args")?) {
                    Ok(mut observation) => {
                        // Stamp after the bounded owner read, not before a slow poll.
                        lua.globals().set("now_ms", now_ms()?)?;
                        sequence = sequence.checked_add(1).ok_or("sequence overflow")?;
                        observation["host_id"] = config["host_id"].clone();
                        observation["instance_id"] = json!(instance);
                        observation["sequence"] = json!(sequence);
                        lua.globals()
                            .set("observation", lua.to_value(&observation)?)?;
                        lua.load(
                            r#"local value=normalizer.normalize(observation,now_ms)
                            publisher:publish({kind=event_kind,body={stream_id=config.stream_id,
                            contract=agent_contract,sequence=value.sequence,
                            observed_at_unix_ms=now_ms,value=value}})"#,
                        )
                        .exec()?;
                    }
                    Err(_) => eprintln!(
                        "agent-runtime: owner source unavailable; retained observations will expire"
                    ),
                }
            }
            next = Instant::now() + Duration::from_secs(10);
        }
        // Receive actual routed events; never feed the UI from direct source output.
        lua.globals().set("now_ms", now_ms()?)?;
        lua.load(r#"
            assert(publisher:status().connected and consumer:status().connected,"Unibus disconnected")
            assert(consumer:status().dropped_events==0 and publisher:status().dropped_events==0,"Unibus overflow; restart/resynchronize")
            for i=1,64 do
                local event=consumer:poll(); if not event then break end
                if event.kind==reply_kind then assert(event.body.success==true,"subscription rejected")
                elseif event.kind==event_kind and event.body.stream_id==config.stream_id
                    and event.body.contract==agent_contract then
                    assert(event.producer_id==config.publisher.producer_id,"unexpected telemetry publisher")
                    adapter.receive(event.body.value,now_ms)
                end
            end
            for i=1,64 do
                local event=publisher:poll(); if not event then break end
                if event.kind==reply_kind then assert(event.body.success==true,"telemetry publication rejected") end
            end
            rendered=adapter.render(now_ms)
        "#).exec()?;
        let mut rendered: Value = lua.from_value(lua.globals().get::<mlua::Value>("rendered")?)?;
        if let Some(stamp) = rendered
            .as_object_mut()
            .and_then(|object| object.remove("observed_at_unix_ms"))
            && let Some(ms) = stamp.as_i64().filter(|ms| *ms > 0)
        {
            rendered["data"]["observed_at"] = json!(
                chrono::DateTime::from_timestamp_millis(ms)
                    .ok_or("invalid observation timestamp")?
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            );
        }
        let output = std::path::Path::new(config["output"].as_str().ok_or("missing output path")?);
        let mut temp =
            tempfile::NamedTempFile::new_in(output.parent().ok_or("missing output parent")?)?;
        use std::io::Write;
        serde_json::to_writer(&mut temp, &rendered)?;
        temp.flush()?;
        temp.persist(output)?;
        std::thread::sleep(Duration::from_secs(1));
    }
}
