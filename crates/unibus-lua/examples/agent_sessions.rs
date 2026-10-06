//! Trusted host stdin -> Lua normalization -> existing authenticated Unibus SDK.
//! No network listener, shell execution, terminal scraping or agent mutations.
use mlua::LuaSerdeExt;
use std::io::{BufRead, Read};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected ClientConfig JSON path")?;
    let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let lua = mlua::Lua::new();
    unibus_lua::install(&lua)?;
    lua.globals().set("reply_kind", unibus::kinds::V1_REPLY)?;
    lua.globals().set("config", lua.to_value(&config)?)?;
    lua.load("bus = unibus.connect(config)").exec()?;
    let module: mlua::Table = lua.load(include_str!("../lua/agent_sessions.lua")).eval()?;
    let publish: mlua::Function = module.get("publish")?;
    let bus: mlua::Value = lua.globals().get("bus")?;
    let mut stdin = std::io::stdin().lock();
    loop {
        // Bound each observation before allocation, not after read_line.
        let mut line = Vec::new();
        let n = stdin
            .by_ref()
            .take(64 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n > 64 * 1024 {
            return Err("owner observation exceeds 64 KiB".into());
        }
        let input: serde_json::Value = serde_json::from_slice(&line)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        let request: String = publish.call((bus.clone(), lua.to_value(&input)?, now))?;
        // Queue acceptance is not router delivery. Surface the correlated reply.
        lua.globals().set("request", request)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let complete: bool = lua.load(r#"
                if not bus:status().connected then error("Unibus disconnected") end
                if bus:status().dropped_events > 0 then error("Unibus replies dropped; resynchronize") end
                local event = bus:poll()
                if not event then return false end
                if event.kind == reply_kind and event.body.request_id == request then
                    assert(event.body.success == true, "router rejected agent telemetry")
                    return true
                end
                return false
            "#).eval()?;
            if complete {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err("router acknowledgement timed out".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(())
}
