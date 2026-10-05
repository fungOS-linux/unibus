//! cargo run -p unibus-lua --example poll -- /path/to/client-config.json
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected ClientConfig JSON path")?;
    let config = std::fs::read_to_string(path)?;
    let lua = mlua::Lua::new();
    unibus_lua::install(&lua)?;
    // Only host-controlled config enters the VM; token values stay in Rust.
    let value: serde_json::Value = serde_json::from_str(&config)?;
    use mlua::LuaSerdeExt;
    lua.globals().set("config", lua.to_value(&value)?)?;
    lua.load("bus = unibus.connect(config)").exec()?;
    loop {
        lua.load(
            r#"
            for i = 1, 16 do
                local event = bus:poll()
                if not event then break end
                print(event.kind, event.producer_id)
            end
            if not bus:status().connected then error("Unibus disconnected") end
        "#,
        )
        .exec()?;
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}
